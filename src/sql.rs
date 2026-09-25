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

use std::sync::Arc;

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

/// A parse-time 22023 (invalid_parameter_value), like PG's
/// anychar_typmodin for bad character-type length modifiers.
fn err_typmod(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "22023",
    }
}

/// v0.28: parse the digits of a PG 16+ non-decimal integer literal (the
/// `0x`/`0o`/`0b` prefix is already stripped). Underscores between digits
/// are ignored, like Postgres. Returns None when there are no digits or
/// the value overflows i64.
fn parse_int_radix(digits: &str, radix: u32) -> Option<i64> {
    let clean: String = digits.chars().filter(|&c| c != '_').collect();
    if clean.is_empty() {
        return None;
    }
    i64::from_str_radix(&clean, radix).ok()
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String), // folded to lowercase unless double-quoted
    /// v0.36: double-quoted identifier, kept verbatim (no case folding).
    /// Quoted identifiers are never keywords, and quoted `"char"` is
    /// PG's one-byte "char" type (OID 18), not `character(1)`.
    QIdent(String),
    Number(String),
    Str(String),
    UStr(String),   // v0.19: U&'...' raw content (UESCAPE handled by parser)
    UIdent(String), // v0.24: U&"..." raw identifier content (UESCAPE handled by parser)
    Param(u32),     // $N parameter placeholder, 1-based
    LParen,
    RParen,
    LBracket, // v0.79: `[` array constructor / subscript / type suffix
    RBracket, // v0.79: `]` array constructor / subscript / type suffix
    Colon,    // v0.79: lone `:` (array slice bounds; `::` stays ColonColon)
    Comma,
    Semi,
    Star,
    Plus,
    Minus,   // v0.7: `-` (unary and binary)
    Slash,   // v0.7: `/`
    Percent, // v0.7: `%`
    Eq,
    Dot,           // v0.6: qualified refs (t.col)
    Lt,            // v0.6: <
    Gt,            // v0.6: >
    LtEq,          // v0.6: <=
    GtEq,          // v0.6: >=
    Neq,           // v0.6: <> and !=
    ColonColon,    // v0.7: `::` cast
    PipePipe,      // v0.7: `||` concat
    Pipe,          // v0.25: `|` bitwise OR
    Amp,           // v0.25: `&` bitwise AND
    Hash,          // v0.25: `#` bitwise XOR
    Tilde,         // v0.25: `~` bitwise NOT (unary)
    TildeStar,     // v0.68: `~*` POSIX regex match, case-insensitive
    BangTilde,     // v0.68: `!~` POSIX regex non-match
    BangTildeStar, // v0.68: `!~*` POSIX regex non-match, case-insensitive
    Shl,           // v0.25: `<<` shift left
    Shr,           // v0.25: `>>` shift right
    At,            // v0.21: `@` prefix abs operator
    PipeSlash,     // v0.21: `|/` prefix sqrt operator
    PipePipeSlash, // v0.21: `||/` prefix cbrt operator
    Caret,         // v0.7: `^` exponentiation
    StarEq,        // v0.81: `*=` PG19 record-image equality (record_image_eq)
    /// v0.86: user-defined operator name starting with `?` (e.g. `?=`).
    /// Only `?`-led sequences lex here; every other operator character
    /// already has its own token. Used in `CREATE OPERATOR`'s name
    /// position; the expression parser rejects it (42601) since custom
    /// operators are not overloadable into expressions.
    Op(String),
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
/// v0.24: PG prohibits certain UESCAPE characters: hex digits, '+',
/// quotes, and whitespace are all invalid ("invalid Unicode escape
/// character").
fn is_valid_uescape(c: char) -> bool {
    !(c.is_ascii_hexdigit() || c == '+' || c == '\'' || c == '"' || c.is_whitespace())
}

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

/// Consume PG19's `{decinteger}` (`{decdigit}(_?{decdigit})*` in
/// `scan.l`): digits with single underscores allowed only *between*
/// digits, so `1_000` lexes as 1000 but `1__2` and `1_` stop at the
/// underscore.
fn consume_dec_digits(chars: &[char], i: &mut usize) {
    let mut prev_was_digit = false;
    while *i < chars.len() {
        let ch = chars[*i];
        if ch.is_ascii_digit() {
            prev_was_digit = true;
            *i += 1;
        } else if ch == '_'
            && prev_was_digit
            && *i + 1 < chars.len()
            && chars[*i + 1].is_ascii_digit()
        {
            *i += 1;
        } else {
            break;
        }
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, SqlError> {
    // A `str`'s char count can never exceed its byte length, so this is a
    // safe upper bound that guarantees `chars` never regrows (Chars'
    // size_hint lower bound is byte_len/4, sized for all-4-byte UTF-8, and
    // badly undershoots for the ASCII-heavy SQL text this tokenizes).
    let mut chars: Vec<char> = Vec::with_capacity(input.len());
    chars.extend(input.chars());
    let mut i = 0;
    // Every token consumes at least one char (comments consume chars but
    // push nothing), plus exactly one more for the trailing `Token::EOF`,
    // so `chars.len() + 1` is a safe upper bound that guarantees `toks`
    // never regrows either.
    let mut toks = Vec::with_capacity(chars.len() + 1);
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
        // `::` cast operator; a lone `:` is the array-slice bound
        // separator (v0.79).
        if c == ':' {
            if i + 1 < chars.len() && chars[i + 1] == ':' {
                toks.push(Token::ColonColon);
                i += 2;
            } else {
                toks.push(Token::Colon);
                i += 1;
            }
            continue;
        }
        // v0.79: `[` and `]` — array constructors, subscripts, slices,
        // and `type[]` suffixes.
        if c == '[' {
            toks.push(Token::LBracket);
            i += 1;
            continue;
        }
        if c == ']' {
            toks.push(Token::RBracket);
            i += 1;
            continue;
        }
        // `||/` prefix cbrt, `|/` prefix sqrt, `||` concatenation,
        // `|` bitwise OR. Longest match first.
        if c == '|' {
            if i + 2 < chars.len() && chars[i + 1] == '|' && chars[i + 2] == '/' {
                toks.push(Token::PipePipeSlash);
                i += 3;
            } else if i + 1 < chars.len() && chars[i + 1] == '/' {
                toks.push(Token::PipeSlash);
                i += 2;
            } else if i + 1 < chars.len() && chars[i + 1] == '|' {
                toks.push(Token::PipePipe);
                i += 2;
            } else {
                toks.push(Token::Pipe);
                i += 1;
            }
            continue;
        }
        // v0.25: `&` bitwise AND, `#` bitwise XOR, `~` bitwise NOT.
        if c == '&' {
            toks.push(Token::Amp);
            i += 1;
            continue;
        }
        if c == '#' {
            toks.push(Token::Hash);
            i += 1;
            continue;
        }
        if c == '~' {
            // v0.68: `~*` lexes as one operator token (PG lexes the
            // multi-char regex operators as single Op tokens); a lone
            // `~` stays the unary bitwise NOT.
            if i + 1 < chars.len() && chars[i + 1] == '*' {
                toks.push(Token::TildeStar);
                i += 2;
            } else {
                toks.push(Token::Tilde);
                i += 1;
            }
            continue;
        }
        // v0.21: `@` prefix absolute-value operator.
        if c == '@' {
            toks.push(Token::At);
            i += 1;
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
        // v0.24: `U&"..."` Unicode escape quoted identifier.
        if (c == 'u' || c == 'U')
            && i + 2 < chars.len()
            && chars[i + 1] == '&'
            && chars[i + 2] == '"'
        {
            i += 2; // consume `U&`, now at the quote
            i += 1; // consume opening `"`
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
            toks.push(Token::UIdent(s));
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
                // v0.81: `*=` is PG19's record-image equality operator.
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::StarEq);
                    i += 2;
                } else {
                    toks.push(Token::Star);
                    i += 1;
                }
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
                if i + 1 < chars.len() && chars[i + 1] == '<' {
                    toks.push(Token::Shl);
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '=' {
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
                if i + 1 < chars.len() && chars[i + 1] == '>' {
                    toks.push(Token::Shr);
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '=' {
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
                } else if i + 1 < chars.len() && chars[i + 1] == '~' {
                    // v0.68: `!~` / `!~*` POSIX regex non-match operators
                    // lex as single tokens, like PG.
                    if i + 2 < chars.len() && chars[i + 2] == '*' {
                        toks.push(Token::BangTildeStar);
                        i += 3;
                    } else {
                        toks.push(Token::BangTilde);
                        i += 2;
                    }
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
                // v0.36: distinct QIdent token so the parser can tell
                // quoted `"char"` (PG's 1-byte type) from unquoted `char`
                // (character(1)); quoted identifiers are never keywords.
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
                toks.push(Token::QIdent(s));
            }
            // v0.24: `$tag$...$tag$` dollar-quoted string, or `$N`
            // parameter placeholder. A `$` that opens neither falls to
            // the syntax error below.
            '$' => {
                // Try a dollar-quote opening delimiter `$tag$`: the tag
                // follows identifier rules (or is empty for `$$`) and may
                // not start with a digit (that would be `$N`).
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let tag: String = chars[i + 1..j].iter().collect();
                // A digit-start tag (e.g. `$1`) is a parameter, never a
                // dollar-quote delimiter.
                let is_open = j < chars.len()
                    && chars[j] == '$'
                    && (tag.is_empty() || !tag.chars().next().unwrap().is_ascii_digit());
                if is_open {
                    let delim: Vec<char> = format!("${}$", tag).chars().collect();
                    let mut k = j + 1;
                    let mut close = None;
                    while k + delim.len() <= chars.len() {
                        if chars[k..k + delim.len()] == delim[..] {
                            close = Some(k);
                            break;
                        }
                        k += 1;
                    }
                    match close {
                        Some(c) => {
                            let content: String = chars[j + 1..c].iter().collect();
                            toks.push(Token::Str(content));
                            i = c + delim.len();
                            continue;
                        }
                        None => {
                            return Err(err("unterminated dollar-quoted string"));
                        }
                    }
                }
                // `$N` parameter placeholder (only when `$` starts the
                // token; `$` inside an identifier, e.g. `a$1`, keeps the
                // old behavior).
                if i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
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
                } else {
                    return Err(err("unexpected character '$'"));
                }
            }
            _ if c.is_ascii_digit()
                || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                let start = i;
                // v0.28: PG 16+ non-decimal integer literals: 0x/0X hex,
                // 0o/0O octal, 0b/0B binary. Underscores may separate digits.
                let radix = if c == '0' && i + 1 < chars.len() {
                    match chars[i + 1] {
                        'x' | 'X' => Some(16u32),
                        'o' | 'O' => Some(8u32),
                        'b' | 'B' => Some(2u32),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(r) = radix {
                    i += 2;
                    // PG: underscores may only appear *between* digits, so
                    // only consume one when a digit follows it.
                    let mut prev_was_digit = false;
                    while i < chars.len() {
                        let ch = chars[i];
                        if ch.is_digit(r) {
                            prev_was_digit = true;
                            i += 1;
                        } else if ch == '_'
                            && prev_was_digit
                            && i + 1 < chars.len()
                            && chars[i + 1].is_digit(r)
                        {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    // v0.28: PG 16+ rejects an identifier char immediately
                    // after a numeric literal ("trailing junk after numeric
                    // literal", 42601) instead of reading it as an alias.
                    if i < chars.len() && (chars[i].is_alphabetic() || chars[i] == '_') {
                        return Err(err("trailing junk after numeric literal"));
                    }
                } else {
                    // v0.40: PG's `{decinteger}` allows single underscores
                    // between digits (`1_000` = 1000), like the radix
                    // branch above.
                    consume_dec_digits(&chars, &mut i);
                    if i < chars.len() && chars[i] == '.' {
                        i += 1;
                        consume_dec_digits(&chars, &mut i);
                    }
                    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                        let mut j = i + 1;
                        if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                            j += 1;
                        }
                        if j < chars.len() && chars[j].is_ascii_digit() {
                            i = j;
                            consume_dec_digits(&chars, &mut i);
                        }
                    }
                    // v0.40: PG 16+ rejects an identifier char immediately
                    // after a decimal numeric literal ("trailing junk after
                    // numeric literal", 42601) instead of reading it as an
                    // alias — the decimal half of the v0.28 check above.
                    if i < chars.len() && (chars[i].is_alphabetic() || chars[i] == '_') {
                        return Err(err("trailing junk after numeric literal"));
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
                let span = &chars[start..i];
                // Fast path: SQL identifiers are ASCII in virtually every
                // real query (Postgres's own unquoted-identifier folding
                // is itself ASCII-only), so build the lowercase text
                // directly in one allocation instead of collecting the
                // original-case text and then `.to_lowercase()`-ing a
                // second String. Non-ASCII identifiers keep the original
                // two-step path exactly, since `to_lowercase()` handles
                // full-Unicode case folding (e.g. context-sensitive Greek
                // final sigma) that a per-char map cannot.
                let word: String = if span.iter().all(|c| c.is_ascii()) {
                    let mut s = String::with_capacity(span.len());
                    for c in span {
                        s.push(c.to_ascii_lowercase());
                    }
                    s
                } else {
                    let raw: String = span.iter().collect();
                    raw.to_lowercase()
                };
                toks.push(Token::Ident(word));
            }
            // v0.86: `?`-led operator names (e.g. `?=` in
            // `CREATE OPERATOR ?=`). `?` previously errored, so no
            // working statement can contain one; lex `?` plus any
            // following operator characters as a single Op token.
            '?' => {
                let start = i;
                i += 1;
                while i < chars.len()
                    && matches!(
                        chars[i],
                        '=' | '?' | '!' | '~' | '@' | '#' | '%' | '^' | '&' | '|'
                    )
                {
                    i += 1;
                }
                let op: String = chars[start..i].iter().collect();
                toks.push(Token::Op(op));
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
    /// A reference count controls the text, thus a literal that runs for
    /// each row makes a `Value::Text` with no copy.
    Text(Arc<str>),
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
            Literal::Decimal(_) => ColType::Numeric(None),
            Literal::Real(_) => ColType::Float4,
            Literal::Numeric(_) => ColType::Numeric(None),
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
            Literal::Text(s) => Value::text(s),
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
    /// v0.81: `*=` — PG19 `record_image_eq` (byte-oriented identity for
    /// fast sorting/grouping; e.g. numeric `1.00` is NOT `*=` `1.0`).
    ImageEq,
}

/// v0.87: the operator in an `Expr::Quantified` — either a builtin
/// comparison or a user-defined operator name (e.g. `?=`).
#[derive(Clone, Debug, PartialEq)]
pub enum QuantOp {
    Cmp(CmpOp),
    User(String),
}

/// v0.87: `ANY`/`SOME` (existential) vs `ALL` (universal) quantification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuantKind {
    Any,
    All,
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
            CmpOp::ImageEq => "*=",
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
    BoolAnd,   // v0.64: bool_and(x)
    // v0.64: variance/stddev. PG's `variance`/`stddev` are the sample
    // forms; `var_samp`/`stddev_samp` are coherent aliases and the
    // `_pop` forms are the population variants.
    VarianceSamp,
    VariancePop,
    StddevSamp,
    StddevPop,
    // v0.91: `array_agg(x)` — collect non-null inputs into a 1-D array
    // (multidimensional when the input is itself an array, like PG).
    ArrayAgg,
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
            AggFunc::BoolAnd => "bool_and",
            AggFunc::VarianceSamp => "variance",
            AggFunc::VariancePop => "var_pop",
            AggFunc::StddevSamp => "stddev",
            AggFunc::StddevPop => "stddev_pop",
            AggFunc::ArrayAgg => "array_agg",
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
    Pow,    // v0.7: `^` exponentiation
    BitAnd, // v0.25: `&`
    BitOr,  // v0.25: `|`
    BitXor, // v0.25: `#`
    Shl,    // v0.25: `<<`
    Shr,    // v0.25: `>>`
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
            ArithOp::BitAnd => "&",
            ArithOp::BitOr => "|",
            ArithOp::BitXor => "#",
            ArithOp::Shl => "<<",
            ArithOp::Shr => ">>",
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
    /// v0.81: cast to a named composite type (`ROW(...)::t_rec`). The
    /// name is resolved against the type catalog at execution time
    /// (the parser is catalog-free); unknown names are 42704 there.
    CastNamed {
        expr: Box<Expr>,
        name: String,
    },
    /// v0.81: `ROW(a, b, ...)` row constructor. Evaluates to
    /// `Value::Record` with PG's `f1`, `f2`, ... field names.
    Row(Vec<Expr>),
    /// v0.81: composite field access, `(expr).field`. The base must
    /// evaluate to a `Value::Record`; unknown fields are 42703.
    FieldAccess {
        expr: Box<Expr>,
        field: String,
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
    /// v0.68: POSIX regex match `~` / `!~` / `~*` / `!~*`
    /// (PG "other native operators" precedence).
    Regex {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        not: bool,
        case_insensitive: bool,
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
    BitNot(Box<Expr>), // v0.25: `~` bitwise NOT
    /// v0.53: unary minus as a first-class operator (PG19 gram.y gives
    /// `-`/`+` UMINUS precedence: tighter than `^`, looser than `::`).
    /// Type-preserving: `-smallint` stays smallint, unlike the old
    /// `0 - x` desugar which widened through integer.
    Neg(Box<Expr>),
    /// v0.55: `CASE [operand] WHEN k THEN r ... [ELSE e] END` (PG19
    /// gram.y: `CASE case_arg when_clause_list case_default END_P`).
    /// `operand` is `None` for the searched form. Each `whens` entry is
    /// `(condition-or-key, result)`.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Box<Expr>, Box<Expr>)>,
        else_: Option<Box<Expr>>,
    },
    IsNull {
        expr: Box<Expr>,
        neg: bool,
    },
    /// v0.48: `a IS [NOT] DISTINCT FROM b` — the NULL-safe comparison
    /// (PG19 gram.y): NULLs compare equal, never unknown.
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
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
        /// v0.92: `ORDER BY` inside the aggregate call (PG19 docs
        /// §4.2.7: allowed in any aggregate; evaluated per input row
        /// before accumulation).
        agg_order_by: Vec<OrderTerm>,
    },
    /// `(SELECT ...)` used as a value: 0 rows -> NULL, >1 row -> 21000.
    ScalarSub(Box<SelectStmt>),
    /// v0.77: `array(SELECT ...)` — ARRAY constructor with a subquery.
    /// Evaluates the subquery and returns the first column of each row
    /// as a PG array literal (e.g. `{1,2,3}`). Currently returned as
    /// Text; a proper array type is future work.
    ArraySubquery(Box<SelectStmt>),
    /// v0.79: `ARRAY[...]` constructor (PG19 `array_expr`). `nested`
    /// marks the `ARRAY[[...],[...]]` form whose elements are themselves
    /// bracketed arrays; an empty element list is the 42P08 "cannot
    /// determine type of empty array" error.
    ArrayCtor {
        elems: Vec<Expr>,
        nested: bool,
    },
    /// v0.79: array subscript (PG19 `A_Indices`). Adjacent bracket
    /// pairs are ONE multidimensional subscript operation (PG19
    /// gram.y / `transformIndirection`: "Adjacent A_Indices nodes
    /// have to be treated as a single multidimensional subscript
    /// operation"), so `a[i][j]` parses to a single node with
    /// `indices = [i, j]`, never nested subscripts. The result type
    /// is the element type; at execution PG19's `array_get_element`
    /// yields NULL unless the index count equals the array's
    /// dimensionality (a partial subscript is NULL, not a subarray).
    Subscript {
        array: Box<Expr>,
        indices: Vec<Expr>,
    },
    /// v0.79: array slice (PG19 `A_Indices` with `is_slice`). If any
    /// bracket in the chain is a slice, the whole chain is a slice
    /// operation (PG19 `array_subscript_transform`): a plain `[i]`
    /// in a slice chain becomes `[1:i]`. Either bound may be absent
    /// (`a[:2]`, `a[1:]`, `a[:]`); absent bounds default to the
    /// array's own bounds, and PG19's `array_get_slice` resets the
    /// result's lower bounds to 1.
    Slice {
        array: Box<Expr>,
        bounds: Vec<(Option<Box<Expr>>, Option<Box<Expr>>)>,
    },
    /// v0.73: PG19 whole-row Var (varattno = 0) — `tbl` or `tbl.*` in
    /// expression position, evaluating to a composite (record) value.
    WholeRow {
        qual: String,
    },
    /// `[NOT] IN (subquery)`. v0.87: `expr` may be an `Expr::Row` for
    /// row-wise `[NOT] IN (SELECT ...)` (PG19).
    InSub {
        expr: Box<Expr>,
        sub: Box<SelectStmt>,
        neg: bool,
    },
    /// v0.87: quantified comparison `expr op ANY|ALL|SOME (subquery)`
    /// (PG19) for operators beyond `= ANY`/`<> ALL` (which desugar to
    /// `InSub`). `left` may be an `Expr::Row` for row-wise quantification.
    /// `op` is a builtin `CmpOp` or a user-defined operator name.
    Quantified {
        left: Box<Expr>,
        op: QuantOp,
        quant: QuantKind,
        sub: Box<SelectStmt>,
    },
    /// v0.87: user-defined binary operator `left OP right` (PG19), e.g.
    /// `?=` from `CREATE OPERATOR`. Resolved at plan time by
    /// (name, leftarg, rightarg); evaluated by calling the operator's
    /// procedure.
    UserOp {
        op: String,
        left: Box<Expr>,
        right: Box<Expr>,
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
    /// v0.23: `[(cols)]` column aliases rename the subquery's output
    /// columns positionally (previously parsed but discarded).
    Derived {
        sub: Box<SelectStmt>,
        alias: String,
        col_aliases: Vec<String>,
    },
    /// v0.14: `(VALUES (e, ...) [, ...]) [AS] alias` — PG names the
    /// columns `column1`, `column2`, ... when no column aliases are given.
    /// v0.21: `[(col, ...)]` column aliases.
    Values {
        rows: Vec<Vec<Expr>>,
        alias: String,
        col_aliases: Vec<String>,
    },
    /// v0.32: `func(args) [AS] alias [(cols)]` — set-returning table
    /// function. v0.46: a function whose args reference earlier FROM
    /// items is evaluated once per left row (implicit LATERAL, PG19).
    /// v0.87: explicit `LATERAL` before a table function is accepted
    /// (PG19); an uncorrelated function under explicit LATERAL is a
    /// plain cross join either way.
    Function {
        name: String,
        args: Vec<Expr>,
        alias: Option<String>,
        col_aliases: Vec<String>,
        lateral: bool,
    },
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
        /// v0.23: `JOIN ... USING (cols) AS alias` — a USING-scoped alias
        /// exposing only the merged columns (the source tables stay
        /// visible). The `AS` keyword is required, like PostgreSQL.
        using_alias: Option<String>,
        /// v0.23: alias for a parenthesized joined table,
        /// `(a JOIN b ...) [AS] x [(cols)]` — hides the inner table names.
        alias: Option<String>,
        /// v0.23: positional column renames for a parenthesized join.
        col_aliases: Vec<String>,
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
    /// v0.52: `SELECT DISTINCT ON (expr [, ...])` (PG19 gram.y
    /// DistinctClause). Empty when absent; mutually exclusive with
    /// `distinct` (the parser enforces it).
    pub distinct_on: Vec<Expr>,
    pub items: Vec<SelectItem>,
    pub from: Vec<FromItem>,
    pub where_: Option<Expr>,
    // v0.78: GROUP BY as a list of grouping sets (PG's grouping-set
    // syntax: `GROUP BY ()`, `ROLLUP`, `CUBE`, `GROUPING SETS`). A
    // plain `GROUP BY a, b` is one set `[[a, b]]`; empty when there is
    // no GROUP BY at all.
    pub group_by: Vec<Vec<Expr>>,
    // v0.78: true when the GROUP BY used grouping-set syntax (one of
    // `()`, `ROLLUP`, `CUBE`, `GROUPING SETS`, or `DISTINCT`). PG then
    // evaluates bare columns that are not in the current grouping set
    // as NULL; a plain GROUP BY instead raises 42803 for them.
    pub group_by_sets: bool,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderTerm>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub for_update: bool,
    /// v0.44: set-operation root when this statement is the carrier of a
    /// `UNION` / `INTERSECT` / `EXCEPT` query. When `Some`, the fields
    /// above are empty/ignored and the query is `set_op`'s branches.
    /// `None` for plain SELECTs (zero behavior change).
    pub set_op: Option<Box<SetOpRoot>>,
}

/// v0.10: one Common Table Expression.
#[derive(Clone, Debug, PartialEq)]
pub struct CteDef {
    pub name: String,
    pub col_aliases: Vec<String>,
    pub body: CteBody,
    pub recursive: bool,
}

/// v0.44: an empty `SelectStmt` shell, used as the carrier of a
/// set-operation root (its query fields stay empty/ignored).
fn empty_select() -> SelectStmt {
    SelectStmt {
        with: Vec::new(),
        distinct: false,
        distinct_on: Vec::new(),
        items: Vec::new(),
        from: Vec::new(),
        where_: None,
        group_by: Vec::new(),
        group_by_sets: false,
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        for_update: false,
        set_op: None,
    }
}

/// v0.44: set-operation kinds for `UNION` / `INTERSECT` / `EXCEPT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

/// v0.44: one `OP [ALL | DISTINCT] <right-branch>` link in a set-operation
/// chain. The right branch is a full `SelectStmt`: it may itself carry a
/// `set_op` (how tighter-precedence `INTERSECT` nests) and its own
/// parenthesized `ORDER BY` / `LIMIT` / `OFFSET`.
#[derive(Clone, Debug, PartialEq)]
pub struct SetOpBranch {
    pub op: SetOpKind,
    pub all: bool,
    pub right: Box<SelectStmt>,
}

/// v0.44: root of a set operation (`SELECT ... UNION/INTERSECT/EXCEPT ...`).
/// Stored on a carrier `SelectStmt` via `SelectStmt::set_op`; the carrier's
/// own query fields (`items`, `from`, ...) are empty and ignored — the
/// branches below are the real queries. Chain links apply left-to-right;
/// `INTERSECT` binds tighter than `UNION`/`EXCEPT` (like Postgres), which
/// the parser achieves by nesting tighter ops inside the right branch.
#[derive(Clone, Debug, PartialEq)]
pub struct SetOpRoot {
    /// Leftmost branch.
    pub left: Box<SelectStmt>,
    /// `(op, all, right)` links, applied left-to-right.
    pub chain: Vec<SetOpBranch>,
    /// Trailing `ORDER BY` / `LIMIT` / `OFFSET`: apply to the combined
    /// result, like Postgres.
    pub order_by: Vec<OrderTerm>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
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

/// v0.22: UPDATE/DELETE graduated to full predicate expressions (see
/// `Stmt::Update.where_`); the legacy `WHERE col = ...` condition list
/// is retired.

/// A value in an INSERT row: a literal, or a `$N` parameter placeholder
/// (v0.3: substituted with the bound value before execution).
#[derive(Clone, Debug, PartialEq)]
pub enum InsertValue {
    Lit(Literal),
    Param(u32),
    /// v0.9: the DEFAULT keyword in INSERT VALUES.
    Default,
    /// v0.24: a general expression in INSERT VALUES
    /// (e.g. `VALUES (repeat('x', 3))`).
    Expr(Expr),
}

/// v0.84: one target of an INSERT column list — a column name with
/// optional PG19 indirection (`f2[1]`, `f3.if2`, `f4[1].if2[1]`).
/// Empty `indirection` is a whole-column target.
#[derive(Clone, Debug, PartialEq)]
pub struct InsertTarget {
    pub name: String,
    pub indirection: Vec<InsertIndirection>,
}

/// v0.84: one indirection step on an INSERT target (PG19 gram.y
/// `opt_indirection` / `indirection_el`, restricted to what INSERT
/// targets can carry).
#[derive(Clone, Debug, PartialEq)]
pub enum InsertIndirection {
    /// `[e1][e2]...` — adjacent bracket pairs merge into ONE step with
    /// several indices (PG19: "Adjacent A_Indices nodes have to be
    /// treated as a single multidimensional subscript operation").
    Index(Vec<Expr>),
    /// `.field`
    Field(String),
    /// `[l:u]` slice — parsed so the error can be PG-shaped; rejected
    /// at execution (0A000, unsupported).
    Slice,
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

/// v0.65: which serial pseudo-type a column was declared with. PG's
/// serial/smallserial/bigserial are not true types: each one creates a
/// backing sequence `<table>_<column>_seq` (type-specific MAXVALUE per
/// PG19 sequence.c: 32767 / 2147483647 / 9223372036854775807), marks the
/// column NOT NULL, and defaults it to `nextval('<table>_<column>_seq')`.
/// An explicit DEFAULT is a 42601 parse-time error in PG19
/// ("multiple default values specified"), not an override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialKind {
    /// `serial` -> integer.
    Serial,
    /// `smallserial` -> smallint.
    SmallSerial,
    /// `bigserial` -> bigint.
    BigSerial,
}

/// v0.77: what a check-like constraint represents. ALTER-added NOT NULL
/// constraints are stored as `col IS NOT NULL` checks (so DROP
/// CONSTRAINT sees them); the kind marker — not the expression shape —
/// decides the 23502-vs-23514 classification in `check_row_constraints`,
/// so an ordinary user CHECK with an `IS NOT NULL` shape can't be
/// misclassified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckKind {
    /// An ordinary CHECK constraint (23514 on violation).
    Check,
    /// An ALTER-added NOT NULL constraint (23502 on violation).
    NotNull,
}

/// v0.9: a CHECK constraint (name + parsed expression).
#[derive(Clone, Debug, PartialEq)]
pub struct CheckDef {
    pub name: String,
    pub expr: Expr,
    /// v0.76: whether the constraint was added NOT VALID (existing rows
    /// were not validated at ALTER time). NOT VALID constraints are still
    /// enforced on new/updated rows by `check_row_constraints`, like PG19;
    /// the flag only skips the one-time existing-row scan.
    pub not_valid: bool,
    pub kind: CheckKind,
}

/// v0.76: a NOT NULL column constraint added via ALTER TABLE
/// (`ADD CONSTRAINT name NOT NULL col [NOT VALID]`).
#[derive(Clone, Debug, PartialEq)]
pub struct NotNullDef {
    pub name: String,
    pub col: String,
    pub not_valid: bool,
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
    /// v0.81: named composite type per column, parallel to `columns`
    /// (`Some(name)` iff the column's `ColType` is `Composite`); resolved
    /// against the type catalog at execution time.
    pub composite_types: Vec<Option<String>>,
    /// v0.85: domain name per column, parallel to `columns`
    /// (`Some(d)` iff the column was declared with domain type `d`,
    /// possibly as `d[]`); resolved against the type catalog at
    /// execution time.
    pub domain_types: Vec<Option<String>>,
    /// v0.85: iff `domain_types[i]` is `Some` and the column was
    /// declared as `d[]` (array of domain): the domain applies per
    /// array element, not to the whole column value.
    pub domain_elem: Vec<bool>,
    pub not_null: Vec<bool>,
    pub defaults: Vec<Option<DefaultExpr>>,
    /// v0.65: per-column serial pseudo-type marker (parallel to
    /// `columns`). Exec creates the backing sequence (type-specific
    /// MAXVALUE, PG19 sequence.c) and wires `DEFAULT nextval(...)` for
    /// marked columns; an explicit DEFAULT is rejected with 42601.
    pub serial: Vec<Option<SerialKind>>,
    /// v0.41: raw `COMPRESSION` option per column, parallel to
    /// `columns` (`None` = not specified). Validated in exec.
    pub compression: Vec<Option<String>>,
    /// v0.72: per-column TOAST storage strategy override, parallel to
    /// `columns` (`None` = type default). Carries `LIKE ...
    /// INCLUDING STORAGE` copies from the source table's `col_storage`.
    pub storage: Vec<Option<u8>>,
    pub checks: Vec<CheckDef>,
    pub uniques: Vec<UniqueDef>,
    pub pkey: Option<UniqueDef>,
    pub fks: Vec<FkDef>,
    /// v0.69: declarative partitioning (`None` = ordinary table).
    pub partition: Option<PartitionDef>,
    /// v0.72: `LIKE` clauses, expanded at exec. PG19 option defaults
    /// (gram.y: a bare LIKE yields options = 0): column names/types are
    /// always copied, NOT NULL constraints are always copied, and every
    /// other kind (DEFAULTS, CONSTRAINTS, INDEXES, STORAGE, COMPRESSION,
    /// COMMENTS, STATISTICS, IDENTITY, GENERATED) is copied only with
    /// the matching INCLUDING option (or INCLUDING ALL).
    pub likes: Vec<LikeClause>,
    /// v0.72: `WITH (storage_parameter = value, ...)` reloptions
    /// (PG19). Recognized parameters are validated at exec; the rest
    /// are accepted and recorded.
    pub reloptions: Vec<(String, String)>,
    /// v0.77: `INHERITS (parent [, ...])` — table inheritance (PG19
    /// transformInhRelation). The child copies the parents' columns at
    /// CREATE time. Querying a parent does NOT yet include children's
    /// rows (no inheritance expansion in scans).
    pub inherits: Vec<String>,
}

/// v0.69: `PARTITION BY` / `PARTITION OF` definition (PG19 partdef.c).
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionDef {
    /// Partitioning method.
    pub method: crate::storage::PartMethod,
    /// Partition key: column names or expressions, in order.
    pub keys: Vec<PartitionKeyDef>,
    /// For `PARTITION OF`: the parent table name.
    pub parent: Option<String>,
    /// For `PARTITION OF` / `ATTACH`: the bound (`None` for a
    /// partitioned root, or when the bound comes from ATTACH).
    pub bound: Option<PartBoundDef>,
}

/// v0.69: one `PARTITION BY` key: a column or a parenthesized expression.
#[derive(Clone, Debug, PartialEq)]
pub enum PartitionKeyDef {
    Column(String),
    Expr(Expr),
}

/// v0.69: `FOR VALUES` bound as parsed (values still `Expr`s; exec
/// coerces them to the parent key column types).
#[derive(Clone, Debug, PartialEq)]
pub enum PartBoundDef {
    List(Vec<Expr>),
    Range {
        lower: Vec<RangeBoundDef>,
        upper: Vec<RangeBoundDef>,
    },
    Hash {
        modulus: u32,
        remainder: u32,
    },
    Default,
}

/// v0.69: one RANGE bound endpoint as parsed.
#[derive(Clone, Debug, PartialEq)]
pub enum RangeBoundDef {
    Min,
    Max,
    Val(Expr),
}

/// v0.9: ALTER TABLE actions.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterAction {
    AddColumn {
        name: String,
        col_type: ColType,
        not_null: bool,
        default: Option<DefaultExpr>,
        /// v0.65: serial pseudo-type marker; exec creates the backing
        /// sequence (type-specific MAXVALUE) and wires DEFAULT
        /// nextval(). An explicit DEFAULT is PG19's 42601
        /// ("multiple default values specified").
        serial: Option<SerialKind>,
        /// v0.41: raw `COMPRESSION` option (`None` = not specified).
        compression: Option<String>,
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
        // v0.76: NOT NULL column constraint.
        notnull: Option<NotNullDef>,
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
    /// v0.37: ALTER TABLE name ALTER COLUMN col SET STORAGE mode.
    /// `mode` is the raw mode name (`plain`/`external`/`extended`/
    /// `main`/`default`); validated in exec against PG's rules.
    SetStorage {
        column: String,
        mode: String,
    },
    /// v0.41: ALTER TABLE name ALTER COLUMN col SET COMPRESSION
    /// method. `mode` is the raw method name (`pglz`/`lz4`/`default`);
    /// validated in exec against PG19's rules (0A000 on non-toastable
    /// types, 22023 on unknown methods).
    SetCompression {
        column: String,
        mode: String,
    },
    /// v0.37: ALTER TABLE name SET (opt = val, ...). Only
    /// `toast_tuple_target` is supported; anything else is 0A000.
    SetRelOptions {
        options: Vec<(String, String)>,
    },
    /// v0.69: `ALTER TABLE parent ATTACH PARTITION child FOR VALUES ...`.
    AttachPartition {
        child: String,
        bound: PartBoundDef,
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
    /// v0.72: `LIKE source_table [like_option ...]` in a CREATE TABLE
    /// column list (PG19 §CREATE TABLE).
    Like(LikeClause),
}

/// v0.72: one `LIKE source_table [like_option ...]` clause. Each option
/// is `INCLUDING`/`EXCLUDING` + one of ALL/COMMENTS/CONSTRAINTS/
/// DEFAULTS/IDENTITY/INDEXES/STATISTICS/STORAGE/GENERATED.
#[derive(Clone, Debug, PartialEq)]
pub struct LikeClause {
    pub source: String,
    pub options: Vec<LikeOption>,
}

/// v0.72: a single `INCLUDING`/`EXCLUDING <kind>` LIKE option.
#[derive(Clone, Debug, PartialEq)]
pub struct LikeOption {
    pub including: bool,
    pub kind: LikeKind,
}

/// v0.72: the LIKE option kinds. PG19 defaults (gram.y: a bare LIKE
/// yields options = 0, i.e. every kind EXCLUDING): only the matching
/// INCLUDING option (or INCLUDING ALL) enables a kind. NOT NULL
/// constraints are copied regardless of options
/// (transformTableLikeClause: "regardless of options given").
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LikeKind {
    All,
    Comments,
    Compression,
    Constraints,
    Defaults,
    Identity,
    Indexes,
    Statistics,
    Storage,
    Generated,
}

struct ParsedColDef {
    name: String,
    col_type: ColType,
    /// v0.81: named composite type for `col_type == ColType::Composite`
    /// (`a t_rec`); resolved against the type catalog at execution time.
    composite_name: Option<String>,
    /// v0.65: serial pseudo-type marker, detected from the raw type
    /// name before it is resolved to a ColType.
    serial: Option<SerialKind>,
    /// v0.41: PG19 `opt_column_compression`: `COMPRESSION method` /
    /// `COMPRESSION DEFAULT` right after the type name. Raw name;
    /// validated in exec (`default` = no explicit method).
    compression: Option<String>,
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
    // v0.76: `NOT NULL col [NOT VALID]` (PG19 ALTER TABLE ADD CONSTRAINT).
    NotNull {
        name: Option<String>,
        col: String,
        not_valid: bool,
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
                Some(Expr::Literal(Literal::Text(s))) => Ok(DefaultExpr::Nextval(s.to_string())),
                _ => Err(err(
                    "nextval() in DEFAULT requires a sequence name string literal".to_string(),
                )),
            }
        }
        other => Ok(DefaultExpr::Expr(other)),
    }
}

impl TableDef {
    pub(crate) fn empty() -> Self {
        TableDef {
            columns: Vec::new(),
            composite_types: Vec::new(),
            domain_types: Vec::new(),
            domain_elem: Vec::new(),
            not_null: Vec::new(),
            defaults: Vec::new(),
            serial: Vec::new(),
            compression: Vec::new(),
            storage: Vec::new(),
            checks: Vec::new(),
            uniques: Vec::new(),
            pkey: None,
            fks: Vec::new(),
            partition: None,
            likes: Vec::new(),
            reloptions: Vec::new(),
            inherits: Vec::new(),
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
        not_valid: false,
        kind: CheckKind::Check,
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
            // v0.81: named composite type travels to exec for catalog
            // resolution.
            def.composite_types.push(c.composite_name.clone());
            // v0.85: domain slots travel alongside; exec resolves the
            // named type to a domain (or not) and fills them in.
            def.domain_types.push(None);
            def.domain_elem.push(false);
            def.not_null.push(false);
            def.defaults.push(None);
            // v0.65: serial marker (with kind) travels to exec for
            // sequence creation.
            def.serial.push(c.serial);
            // v0.41: raw COMPRESSION option travels with the column.
            def.compression.push(c.compression.clone());
            // v0.72: no column-level STORAGE syntax yet; LIKE ...
            // INCLUDING STORAGE fills this at exec.
            def.storage.push(None);
        }
    }
    // v0.72: a bare `(LIKE src)` list contributes its columns at exec;
    // only reject a list with neither columns nor LIKE clauses.
    // (Checked against the raw items: pass 2 below is what fills
    // def.likes.)
    // v0.74: zero-column tables (`CREATE TABLE t()`) are legal in
    // PostgreSQL; only a list that intended columns but produced none
    // (and no LIKE) is an error.
    let has_like = items.iter().any(|i| matches!(i, TableItem::Like(_)));
    if def.columns.is_empty() && !has_like && !items.is_empty() {
        return Err(err(
            "syntax error: table must have at least one column".to_string()
        ));
    }
    // Pass 2: constraints.
    for item in &items {
        match item {
            // v0.72: LIKE clauses are collected verbatim; exec expands
            // them against the source table (needs catalog access).
            TableItem::Like(lc) => {
                def.likes.push(lc.clone());
            }
            TableItem::Col(c) => {
                let i = def.columns.iter().position(|(n, _)| n == &c.name).unwrap();
                // v0.65: a serial column is implicitly NOT NULL (PG's
                // transformColumnDefinition). An explicit NULL conflicts.
                if c.serial.is_some() {
                    def.not_null[i] = true;
                }
                for con in &c.cons {
                    match con {
                        ColCon::NotNull => def.not_null[i] = true,
                        ColCon::Null => {
                            if c.serial.is_some() {
                                return Err(err(format!(
                                    "conflicting NULL/NOT NULL declarations for column \"{}\" of table \"{}\"",
                                    c.name, table
                                )));
                            }
                            def.not_null[i] = false
                        }
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
                // v0.76: `CONSTRAINT name NOT NULL col` in CREATE TABLE —
                // PG doesn't allow this syntax here, but we accept it by
                // marking the column NOT NULL (like a column constraint).
                ParsedTableCon::NotNull { col, .. } => {
                    if let Some(i) = def.columns.iter().position(|(n, _)| n == col) {
                        def.not_null[i] = true;
                    } else {
                        return Err(err(format!(
                            "syntax error: column \"{}\" of relation \"{}\" does not exist",
                            col, table
                        )));
                    }
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
        // v0.73: a whole-row ref depends on every column of the range.
        Expr::WholeRow { qual } => out.push((Some(qual.clone()), "*".to_string())),
        Expr::Arith { left, right, .. } => {
            collect_col_refs(left, out);
            collect_col_refs(right, out);
        }
        Expr::Cast { expr, .. } => collect_col_refs(expr, out),
        // v0.81: composite expressions — recurse into sub-expressions.
        Expr::CastNamed { expr, .. } => collect_col_refs(expr, out),
        Expr::Row(elems) => {
            for e in elems {
                collect_col_refs(e, out);
            }
        }
        Expr::FieldAccess { expr, .. } => collect_col_refs(expr, out),
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            collect_col_refs(a, out);
            collect_col_refs(b, out);
        }
        Expr::Not(a) => collect_col_refs(a, out),
        Expr::BitNot(a) => collect_col_refs(a, out),
        Expr::Neg(a) => collect_col_refs(a, out),
        // v0.55: CASE — collect from every arm.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_col_refs(o, out);
            }
            for (k, r) in whens {
                collect_col_refs(k, out);
                collect_col_refs(r, out);
            }
            if let Some(e) = else_ {
                collect_col_refs(e, out);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            collect_col_refs(expr, out);
            collect_col_refs(pattern, out);
        }
        // v0.68: regex match visits both operands like LIKE.
        Expr::Regex { expr, pattern, .. } => {
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
        Expr::IsDistinctFrom { left, right, .. } => {
            collect_col_refs(left, out);
            collect_col_refs(right, out);
        }
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
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Exists { .. }
        | Expr::ResolvedCol { .. } => {}
        // v0.87: quantified comparison — the left side is outer-scope
        // (the subquery has its own scope, like InSub).
        Expr::Quantified { left, .. } => collect_col_refs(left, out),
        Expr::UserOp { left, right, .. } => {
            collect_col_refs(left, out);
            collect_col_refs(right, out);
        }
        // v0.79: array expressions — collect from elements/operands.
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                collect_col_refs(e, out);
            }
        }
        Expr::Subscript { array, indices } => {
            collect_col_refs(array, out);
            for i in indices {
                collect_col_refs(i, out);
            }
        }
        Expr::Slice { array, bounds } => {
            collect_col_refs(array, out);
            for (l, u) in bounds {
                if let Some(l) = l {
                    collect_col_refs(l, out);
                }
                if let Some(u) = u {
                    collect_col_refs(u, out);
                }
            }
        }
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

/// v0.86: a single `CREATE FUNCTION` argument: the optional argument
/// name and the declared type name as written.
#[derive(Clone, Debug)]
pub struct FuncArg {
    pub name: Option<String>,
    pub type_name: String,
}

/// v0.86: function languages we can execute. Anything else (plpgsql,
/// C, ...) is rejected at parse time with 42601.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncLang {
    Sql,
    Internal,
}

/// v0.86: `VOLATILE` / `STABLE` / `IMMUTABLE` markers. Stored for
/// catalog fidelity; the executor does not yet reorder or cache on
/// volatility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncVolatility {
    Volatile,
    Stable,
    Immutable,
}

/// v0.88: one key column of `CREATE INDEX`. Either a plain column
/// (`name`) or an index expression (`expr`, the source text — stored
/// for catalog fidelity; the v0.88 planner does not evaluate it).
#[derive(Clone, Debug)]
pub struct IndexColSpec {
    pub name: Option<String>,
    pub expr: Option<String>,
    pub desc: bool,
    pub nulls_first: bool,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    CreateTable {
        name: String,
        def: TableDef,
        /// v0.21: true for `CREATE TEMP TABLE` — the table is dropped
        /// if it already exists (session-local semantics approximation
        /// for pg_regress conformance).
        temp: bool,
    },
    /// v0.48: `CREATE TABLE ... AS <query>` (CTAS, PG19 createas.c):
    /// the table's columns are inferred from the query's output names
    /// and types; explicit aliases rename them positionally.
    CreateTableAs {
        name: String,
        col_aliases: Vec<String>,
        select: Box<SelectStmt>,
        temp: bool,
        if_not_exists: bool,
        /// `WITH NO DATA` creates the table without populating it.
        with_data: bool,
    },
    // --- v0.9: ALTER TABLE ---
    AlterTable {
        name: String,
        /// v0.72: PG19 allows a comma-separated list of actions in one
        /// ALTER TABLE; they run left to right in a single statement.
        actions: Vec<AlterAction>,
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
    // v0.75: CREATE STATISTICS (extended statistics). Parsed and accepted
    // as a no-op; the statistics are not used by the planner.
    CreateStatistics,
    // --- v0.22: CREATE TYPE (bounded shell-type support) ---
    CreateType {
        name: String,
        /// Base type from LIKE = <base> in the parenthesized completion
        /// form; None for the bare `CREATE TYPE name;` shell form.
        like_base: Option<String>,
        /// v0.81: `CREATE TYPE name AS (field type, ...)` composite
        /// definition: `(field_name, ColType, composite_type_name)`.
        /// None for the shell and LIKE forms.
        composite: Option<Vec<(String, ColType, Option<String>)>>,
    },
    DropType {
        names: Vec<String>,
        if_exists: bool,
    },
    // --- v0.85: CREATE DOMAIN (bounded): a named type over a base
    // type with optional CHECK constraints, NOT NULL, and DEFAULT.
    // `base_named` carries the named type for a composite or domain
    // base (None for builtins); resolved against the type catalog at
    // execution time.
    CreateDomain {
        name: String,
        base: ColType,
        base_named: Option<String>,
        checks: Vec<CheckDef>,
        not_null: bool,
        default: Option<DefaultExpr>,
    },
    DropDomain {
        names: Vec<String>,
        if_exists: bool,
    },
    // --- v0.86: CREATE FUNCTION (bounded): SQL-language and internal
    // functions. Only `LANGUAGE sql` and `LANGUAGE internal` are
    // supported; any other language (plpgsql, C, ...) is rejected at
    // parse time with 42601. `args` carries the optional argument
    // names (for named references like `t.col` in the body) and the
    // declared type names. `body` is the raw function-body string;
    // the executor parses it once at CREATE time. `or_replace`
    // implements CREATE OR REPLACE (42723 without it on duplicates).
    CreateFunction {
        name: String,
        args: Vec<FuncArg>,
        ret_type: String,
        returns_set: bool,
        lang: FuncLang,
        body: String,
        or_replace: bool,
        volatility: FuncVolatility,
        strict: bool,
    },
    DropFunction {
        name: String,
        arg_types: Vec<String>,
        if_exists: bool,
        /// v0.87: PG19 CASCADE/RESTRICT (RESTRICT is the default).
        cascade: bool,
    },
    /// v0.86: `DROP OPERATOR [IF EXISTS] name (lefttype, righttype)`.
    DropOperator {
        name: String,
        leftarg: Option<String>,
        rightarg: Option<String>,
        if_exists: bool,
        /// v0.87: PG19 CASCADE/RESTRICT (RESTRICT is the default).
        cascade: bool,
    },
    // --- v0.86: CREATE OPERATOR (bounded): registers a user-defined
    // operator name mapping to a function (`PROCEDURE`). Only the
    // equality use in `[NOT] IN` subqueries is wired to user operators;
    // general expression use stays 42601.
    CreateOperator {
        name: String,
        procedure: String,
        leftarg: Option<String>,
        rightarg: Option<String>,
        commutator: Option<String>,
        negator: Option<String>,
        hashes: bool,
        merges: bool,
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
        /// v0.84: targets carry PG19 indirection (`f2[1]`, `f3.if2`).
        columns: Option<Vec<InsertTarget>>,
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
        /// v0.76: optional table alias (`UPDATE t AS x ...`).
        alias: Option<String>,
        /// (column, expression) assignments.
        sets: Vec<(String, Expr)>,
        /// v0.76: `FROM` items (PG19 `UPDATE ... FROM`).
        from: Vec<FromItem>,
        /// v0.22: full predicate expression (was `Vec<WhereCond>` limited
        /// to `col = literal`). Parsed with `parse_or`, like SELECT's
        /// WHERE and ON CONFLICT DO UPDATE's WHERE.
        where_: Option<Expr>,
        /// v0.10: `RETURNING ...`.
        returning: Vec<SelectItem>,
        /// v0.10: `WITH ...` CTEs visible to the statement.
        with: Vec<CteDef>,
    },
    Delete {
        table: String,
        /// v0.65: optional table alias (`DELETE FROM t AS dt`); the
        /// alias is the qualifier visible in WHERE/RETURNING, like PG.
        alias: Option<String>,
        /// v0.74: `USING <from-items>` — extra tables the WHERE clause
        /// may reference (PG19), like SELECT's FROM.
        using: Vec<FromItem>,
        /// v0.22: full predicate expression (was `Vec<WhereCond>`).
        where_: Option<Expr>,
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
        columns: Vec<IndexColSpec>,
        unique: bool,
        if_not_exists: bool,
        /// v0.88: partial-index predicate source (`WHERE ...`), if any.
        predicate: Option<String>,
    },
    DropIndex {
        names: Vec<String>,
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
    /// `SET name = value` / `SET name TO value` / `SET LOCAL ...` —
    /// session GUC. `local` is true for `SET LOCAL`, which is
    /// transaction-scoped: the value reverts at transaction end (PG19).
    Set {
        name: String,
        value: SetValue,
        local: bool,
    },
    /// `SHOW name`.
    Show {
        name: String,
    },
    /// `RESET name` / `RESET ALL`.
    Reset {
        name: String,
    },
    /// v0.74: `PREPARE name [(type, ...)] AS statement` (PG19) — a
    /// session-level named prepared statement; the inner statement may
    /// reference `$N` parameters bound by EXECUTE.
    Prepare {
        name: String,
        /// Declared parameter types (for arity checking at EXECUTE).
        types: Vec<String>,
        stmt: Box<Stmt>,
    },
    /// v0.74: `EXECUTE name [(expr, ...)]` (PG19) — run a statement
    /// created by SQL-level PREPARE with the argument values bound.
    Execute {
        name: String,
        args: Vec<Expr>,
    },
    /// v0.74: `DEALLOCATE name` / `DEALLOCATE ALL` (PG19).
    Deallocate {
        /// None = ALL.
        name: Option<String>,
    },
}

/// v0.10: `INSERT ... ON CONFLICT ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    pub arbiter: ConflictArbiter,
    pub action: ConflictAction,
}

/// v0.81: true for statements that change the catalog (DDL). The server
/// bumps `Engine::catalog_epoch` after one of these succeeds, which
/// invalidates every session's parsed-AST cache (PostgreSQL invalidates
/// its plan cache on catalog changes the same way).
impl Stmt {
    pub fn is_catalog_changing(&self) -> bool {
        matches!(
            self,
            Stmt::CreateTable { .. }
                | Stmt::CreateTableAs { .. }
                | Stmt::AlterTable { .. }
                | Stmt::DropTable { .. }
                | Stmt::CreateView { .. }
                | Stmt::DropView { .. }
                | Stmt::CreateSequence { .. }
                | Stmt::AlterSequence { .. }
                | Stmt::DropSequence { .. }
                | Stmt::CreateType { .. }
                | Stmt::DropType { .. }
                | Stmt::CreateDomain { .. }
                | Stmt::DropDomain { .. }
                | Stmt::CreateIndex { .. }
                | Stmt::DropIndex { .. }
                | Stmt::CreateStatistics
                | Stmt::CreateRole { .. }
                | Stmt::AlterRole { .. }
                | Stmt::DropRole { .. }
        )
    }
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
                from,
                where_,
                returning,
                with,
                ..
            } => {
                let mut m = 0;
                for (_, e) in sets {
                    m = m.max(max_param_expr(e));
                }
                // v0.76: UPDATE ... FROM items may hold parameters
                // (derived tables, functions, VALUES).
                for f in from {
                    m = m.max(max_param_from(f));
                }
                if let Some(w) = where_ {
                    m = m.max(max_param_expr(w));
                }
                m = m.max(max_param_returning(returning));
                m = m.max(max_param_ctes(with));
                m
            }
            Stmt::Delete {
                using,
                where_,
                returning,
                with,
                ..
            } => {
                let mut m = 0;
                // v0.76: DELETE ... USING items may hold parameters.
                for f in using {
                    m = m.max(max_param_from(f));
                }
                if let Some(w) = where_ {
                    m = m.max(max_param_expr(w));
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
    for e in s.group_by.iter().flatten() {
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
        // v0.32: table-function args may hold $n parameters.
        FromItem::Function { args, .. } => args.iter().map(max_param_expr).max().unwrap_or(0),
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
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::WholeRow { .. }
        | Expr::Literal(_) => 0,
        Expr::Arith { left, right, .. } | Expr::And(left, right) | Expr::Or(left, right) => {
            max_param_expr(left).max(max_param_expr(right))
        }
        Expr::Concat(a, b) => max_param_expr(a).max(max_param_expr(b)),
        Expr::Cmp { left, right, .. } => max_param_expr(left).max(max_param_expr(right)),
        // v0.81: composite expressions recurse into their operands.
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => max_param_expr(expr),
        Expr::Row(elems) => elems.iter().map(max_param_expr).max().unwrap_or(0),
        // v0.79: array expressions recurse into their operands.
        Expr::ArrayCtor { elems, .. } => elems.iter().map(max_param_expr).max().unwrap_or(0),
        Expr::Subscript { array, indices } => {
            max_param_expr(array).max(indices.iter().map(max_param_expr).max().unwrap_or(0))
        }
        Expr::Slice { array, bounds } => bounds.iter().fold(max_param_expr(array), |m, (l, u)| {
            m.max(l.as_ref().map(|l| max_param_expr(l)).unwrap_or(0))
                .max(u.as_ref().map(|u| max_param_expr(u)).unwrap_or(0))
        }),
        Expr::Like { expr, pattern, .. } => max_param_expr(expr).max(max_param_expr(pattern)),
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => max_param_expr(expr).max(max_param_expr(pattern)),
        Expr::Between {
            expr, low, high, ..
        } => max_param_expr(expr)
            .max(max_param_expr(low))
            .max(max_param_expr(high)),
        Expr::Cast { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr)
        | Expr::IsNull { expr, .. } => max_param_expr(expr),
        // v0.55: CASE — max over every arm.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            let mut m = operand.as_deref().map(max_param_expr).unwrap_or(0);
            for (k, r) in whens {
                m = m.max(max_param_expr(k)).max(max_param_expr(r));
            }
            if let Some(e) = else_ {
                m = m.max(max_param_expr(e));
            }
            m
        }
        Expr::IsBool { expr, .. } => max_param_expr(expr),
        Expr::IsDistinctFrom { left, right, .. } => max_param_expr(left).max(max_param_expr(right)),
        Expr::Func { args, .. } => args.iter().map(max_param_expr).max().unwrap_or(0),
        Expr::Extract { from, .. } => max_param_expr(from),
        Expr::Agg { arg, arg2, .. } => arg
            .as_deref()
            .map(max_param_expr)
            .unwrap_or(0)
            .max(arg2.as_deref().map(max_param_expr).unwrap_or(0)),
        Expr::ScalarSub(s) => max_param_select(s),
        Expr::ArraySubquery(s) => max_param_select(s),
        Expr::InSub { expr, sub, .. } => max_param_expr(expr).max(max_param_select(sub)),
        // v0.87: quantified comparison and user operator.
        Expr::Quantified { left, sub, .. } => max_param_expr(left).max(max_param_select(sub)),
        Expr::UserOp { left, right, .. } => max_param_expr(left).max(max_param_expr(right)),
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
            | "except" // v0.44: set operations
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
            | "intersect" // v0.44: set operations
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

/// v0.88: render a token slice back to SQL-ish source text, for stored
/// index expressions / partial-index predicates (catalog fidelity
/// only — never re-parsed for evaluation). Spacing is canonical, not
/// verbatim: `a * a`, `id1 % 1000 = 1`.
fn tokens_to_sql(toks: &[Token]) -> String {
    fn text(t: &Token) -> String {
        match t {
            Token::Ident(s) => s.clone(),
            Token::QIdent(s) => format!("\"{}\"", s.replace('"', "\"\"")),
            Token::Number(s) => s.clone(),
            Token::Str(s) => format!("'{}'", s.replace('\'', "''")),
            Token::UStr(s) => format!("U&'{}'", s.replace('\'', "''")),
            Token::UIdent(s) => format!("U&\"{}\"", s.replace('"', "\"\"")),
            Token::Param(n) => format!("${}", n),
            Token::Op(s) => s.clone(),
            Token::LParen => "(".to_string(),
            Token::RParen => ")".to_string(),
            Token::LBracket => "[".to_string(),
            Token::RBracket => "]".to_string(),
            Token::Colon => ":".to_string(),
            Token::Comma => ",".to_string(),
            Token::Semi => ";".to_string(),
            Token::Star => "*".to_string(),
            Token::Plus => "+".to_string(),
            Token::Minus => "-".to_string(),
            Token::Slash => "/".to_string(),
            Token::Percent => "%".to_string(),
            Token::Eq => "=".to_string(),
            Token::Dot => ".".to_string(),
            Token::Lt => "<".to_string(),
            Token::Gt => ">".to_string(),
            Token::LtEq => "<=".to_string(),
            Token::GtEq => ">=".to_string(),
            Token::Neq => "<>".to_string(),
            Token::ColonColon => "::".to_string(),
            Token::PipePipe => "||".to_string(),
            Token::Pipe => "|".to_string(),
            Token::Amp => "&".to_string(),
            Token::Hash => "#".to_string(),
            Token::Tilde => "~".to_string(),
            Token::TildeStar => "~*".to_string(),
            Token::BangTilde => "!~".to_string(),
            Token::BangTildeStar => "!~*".to_string(),
            Token::Shl => "<<".to_string(),
            Token::Shr => ">>".to_string(),
            Token::At => "@".to_string(),
            Token::PipeSlash => "|/".to_string(),
            Token::PipePipeSlash => "||/".to_string(),
            Token::Caret => "^".to_string(),
            Token::StarEq => "*=".to_string(),
            Token::EOF => String::new(),
        }
    }
    /// No space before these (closers and infix punctuation).
    fn no_space_before(t: &Token) -> bool {
        matches!(
            t,
            Token::RParen
                | Token::Comma
                | Token::Semi
                | Token::Dot
                | Token::RBracket
                | Token::ColonColon
                | Token::EOF
        )
    }
    /// No space after these (openers and prefix punctuation).
    fn no_space_after(t: &Token) -> bool {
        matches!(
            t,
            Token::LParen | Token::LBracket | Token::Dot | Token::ColonColon | Token::At
        )
    }
    let mut out = String::new();
    let mut prev: Option<&Token> = None;
    for t in toks {
        if let Token::EOF = t {
            break;
        }
        if !out.is_empty() && !no_space_before(t) && !prev.is_some_and(no_space_after) {
            out.push(' ');
        }
        out.push_str(&text(t));
        prev = Some(t);
    }
    out
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// v0.14: counter for auto-generated `unnamed_subquery[_N]` aliases,
    /// matching what PostgreSQL reports for alias-less FROM subqueries.
    unnamed_seq: usize,
    /// v0.31: when false, `parse_cmp` does not consume `SIMILAR` as the
    /// SIMILAR TO operator. Used while parsing the subject of
    /// `SUBSTRING(s SIMILAR pat ...)`, where a bare SIMILAR introduces the
    /// SQL substring form instead of a boolean SIMILAR TO test.
    allow_similar_to: bool,
}

impl Parser {
    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::EOF)
    }

    /// Token after the next one (for `qual.*` / `NOT IN` lookahead).
    fn peek2(&self) -> &Token {
        self.tokens.get(self.pos + 1).unwrap_or(&Token::EOF)
    }

    /// Third token (for `qual.*` vs `qual.col` disambiguation).
    fn peek3(&self) -> &Token {
        self.tokens.get(self.pos + 2).unwrap_or(&Token::EOF)
    }

    /// v0.75: scan ahead from the current position (which must be at a
    /// `(` token) for a UNION/INTERSECT/EXCEPT at this paren depth, i.e.
    /// `((SELECT ...) UNION ...)` rather than a parenthesized expression.
    /// String tokens are opaque; paren depth is tracked structurally.
    fn paren_has_top_level_setop(&self) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos;
        while let Some(tok) = self.tokens.get(i) {
            match tok {
                Token::LParen => depth += 1,
                Token::RParen => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                Token::Ident(s)
                    if depth == 1 && (s == "union" || s == "intersect" || s == "except") =>
                {
                    return true;
                }
                Token::EOF => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// v0.36: the text of a peeked identifier, whether bare (folded) or
    /// double-quoted (verbatim). Used at identifier positions where both
    /// forms are legal names; keyword checks keep matching `Ident` only.
    fn peek_ident(&self) -> Option<&str> {
        match self.peek() {
            Token::Ident(s) | Token::QIdent(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        match self.tokens.get(self.pos) {
            Some(Token::Ident(s)) if s == kw => {
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

    /// v0.24: parse the optional `UESCAPE 'c'` clause after a `U&...`
    /// literal/identifier. Returns the escape char (default `\`).
    /// PG rejects hex digits, '+', quotes, and whitespace as the
    /// escape character.
    fn parse_uescape(&mut self) -> Result<char, SqlError> {
        if !self.eat_keyword("uescape") {
            return Ok('\\');
        }
        match self.next() {
            Token::Str(e) => {
                let mut ch = e.chars();
                match (ch.next(), ch.next()) {
                    (Some(c), None) if is_valid_uescape(c) => Ok(c),
                    (Some(_), None) => Err(err("invalid Unicode escape character")),
                    _ => Err(err("UESCAPE string must be empty or one character")),
                }
            }
            other => Err(err(format!(
                "syntax error: expected string literal for UESCAPE, found {:?}",
                other
            ))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, SqlError> {
        match self.next() {
            Token::Ident(s) => Ok(s),
            // v0.36: double-quoted identifiers are valid anywhere a name
            // is expected; the verbatim text is the name.
            Token::QIdent(s) => Ok(s),
            Token::UIdent(raw) => {
                // v0.24: U&"..." with optional UESCAPE 'c' clause.
                let escape = self.parse_uescape()?;
                match decode_ustr(&raw, escape) {
                    Some(t) => Ok(t),
                    None => Err(err("invalid Unicode escape")),
                }
            }
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

    /// v0.69: expect an unsigned integer literal (for MODULUS/REMAINDER).
    fn expect_u32(&mut self) -> Result<u32, SqlError> {
        match self.next() {
            Token::Number(s) => s.parse::<u32>().map_err(|_| {
                err(format!(
                    "syntax error: expected unsigned integer, found {}",
                    s
                ))
            }),
            other => Err(err(format!(
                "syntax error: expected unsigned integer, found {:?}",
                other
            ))),
        }
    }

    /// v0.37: read a reloption value (`SET (opt = val, ...)`). Accepts an
    /// identifier, number, or string literal; returns the raw text.
    /// v0.72: parse a `(opt = val, ...)` reloptions list (shared by
    /// CREATE TABLE ... WITH (...) and ALTER TABLE ... SET (...)).
    fn parse_reloptions(&mut self) -> Result<Vec<(String, String)>, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let mut options = Vec::new();
        loop {
            let opt_name = self.expect_ident()?;
            self.expect(Token::Eq, "'='")?;
            // Option values are simple: identifiers, numbers, or
            // string literals. Read the raw token text.
            let opt_val = self.expect_reloption_value()?;
            options.push((opt_name, opt_val));
            if *self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        self.expect(Token::RParen, "')'")?;
        Ok(options)
    }

    fn expect_reloption_value(&mut self) -> Result<String, SqlError> {
        match self.next() {
            Token::Ident(s) | Token::QIdent(s) | Token::Number(s) | Token::Str(s) => Ok(s),
            other => Err(err(format!(
                "syntax error: expected option value, found {:?}",
                other
            ))),
        }
    }

    fn parse_top(&mut self) -> Result<Stmt, SqlError> {
        let kw = match self.next() {
            Token::Ident(s) => s,
            // v0.44: a top-level parenthesized query, e.g.
            // `(SELECT 1 UNION SELECT 2) UNION SELECT 3`.
            Token::LParen => {
                let inner = self.parse_select_query_not_consumed()?;
                self.expect(Token::RParen, "')'")?;
                // A parenthesized query may be followed by set operators.
                let carrier = self.parse_set_chain(inner, 1)?;
                let stmt = self.finish_select_query(carrier)?;
                return Ok(Stmt::Select(stmt));
            }
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
            "select" => Ok(Stmt::Select(self.parse_select_query()?)),
            // v0.54: PG19 `simple_select: values_clause | TABLE relation_expr`
            // — VALUES and TABLE are full top-level queries, not just
            // FROM items. Both desugar to the SELECT the executor
            // already handles, then flow through set-ops and the tail.
            "values" => Ok(Stmt::Select(self.parse_values_query()?)),
            "table" => Ok(Stmt::Select(self.parse_table_query()?)),
            "drop" => self.parse_drop(),
            // --- v0.11: GRANT / REVOKE
            "grant" => self.parse_grant(),
            "revoke" => self.parse_revoke(),
            // --- v0.5: UPDATE / DELETE
            "update" => self.parse_update(),
            "delete" => self.parse_delete(),
            // --- v0.74: PREPARE / EXECUTE / DEALLOCATE
            "prepare" => self.parse_prepare(),
            "execute" => self.parse_execute(),
            "deallocate" => self.parse_deallocate(),
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
                // v0.77: EXPLAIN (option, ...) — parse and ignore the options
                // (COSTS, VERBOSE, BUFFERS, TIMING, SUMMARY, SETTINGS, FORMAT).
                // The options don't affect rustgres's plan output format, but
                // accepting the syntax avoids a 42601 that would (correctly)
                // abort an explicit transaction in the conformance suite.
                if matches!(self.peek(), Token::LParen) {
                    let _ = self.next(); // consume '('
                    loop {
                        // Option name: identifier.
                        match self.next() {
                            Token::Ident(_) => {}
                            other => {
                                return Err(err(format!(
                                    "syntax error: unexpected {:?} in EXPLAIN options",
                                    other
                                )));
                            }
                        }
                        // Optional value: boolean keyword, number, string, or identifier.
                        match self.peek() {
                            Token::Number(_) | Token::Str(_) | Token::Ident(_) => {
                                let _ = self.next();
                            }
                            _ => {}
                        }
                        match self.next() {
                            Token::Comma => continue,
                            Token::RParen => break,
                            other => {
                                return Err(err(format!(
                                    "syntax error: unexpected {:?} in EXPLAIN options",
                                    other
                                )));
                            }
                        }
                    }
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
                    Token::Ident(_) | Token::QIdent(_) => Some(self.expect_ident()?),
                    other => {
                        return Err(err(format!("syntax error: unexpected {:?}", other)));
                    }
                };
                Ok(Stmt::Analyze { table })
            }
            // --- v0.9: ALTER TABLE / ALTER SEQUENCE
            "alter" => match self.peek() {
                Token::Ident(s) if s == "table" => self.parse_alter(),
                Token::Ident(s) if s == "sequence" => self.parse_alter_sequence(),
                Token::Ident(s) if s == "role" || s == "user" || s == "group" => {
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

    fn parse_col_type(&mut self) -> Result<(ColType, Option<String>), SqlError> {
        self.parse_type_name()
    }

    /// A type name for CREATE TABLE / CAST / `::` (v0.7: full set).
    /// Multi-word names like `double precision` and
    /// `timestamp with time zone` are accepted; `numeric(p[,s])`
    /// precision/scale are parsed but not enforced (documented).
    /// v0.81: returns the resolved `ColType` plus, for a name that is
    /// not a builtin, `Some(name)` with `ColType::Composite` — the name
    /// is resolved against the type catalog at execution time (42704 if
    /// undefined, like PG's "type does not exist").
    fn parse_type_name(&mut self) -> Result<(ColType, Option<String>), SqlError> {
        // v0.36: a double-quoted name resolves case-sensitively; quoted
        // `"char"` is PG's one-byte "char" type (OID 18), not character(1).
        if let Token::QIdent(name) = self.peek() {
            let name = name.clone();
            self.next();
            return self.parse_quoted_type_name(name);
        }
        let name = self.expect_ident()?;
        // v0.75: schema-qualified type name (`pg_catalog.int4`,
        // `information_schema.sql_identifier`); the schema qualifier is
        // accepted and ignored.
        let name = if *self.peek() == Token::Dot && matches!(self.peek2(), Token::Ident(_)) {
            self.next(); // '.'
            self.expect_ident()?
        } else {
            name
        };
        self.parse_type_name_rest(name)
    }

    /// v0.36: resolve a double-quoted type name. Quoted `"char"` is PG's
    /// one-byte "char" type (OID 18), which takes no typmod; every other
    /// name resolves exactly like its unquoted spelling (so `"text"`,
    /// `"bpchar"` keep working, while `"CHAR"` stays unknown, like PG).
    fn parse_quoted_type_name(
        &mut self,
        name: String,
    ) -> Result<(ColType, Option<String>), SqlError> {
        if name == "char" {
            if *self.peek() == Token::LParen {
                return Err(err(
                    "syntax error: type modifier is not allowed for type \"char\"".to_string(),
                ));
            }
            return Ok((ColType::SingleChar, None));
        }
        self.parse_type_name_rest(name)
    }

    fn parse_type_name_rest(
        &mut self,
        name: String,
    ) -> Result<(ColType, Option<String>), SqlError> {
        let base: ColType = match name.as_str() {
            "int" | "integer" => Ok(ColType::Int),
            // v0.14: PostgreSQL internal alias names (pg_regress conformance).
            "int4" => Ok(ColType::Int),
            "bigint" | "int8" => Ok(ColType::BigInt),
            "smallint" | "int2" => Ok(ColType::SmallInt),
            // v0.14: `serial` accepted as an integer alias for casts.
            // v0.65: in column definitions all three serial pseudo-types
            // now create a real backing sequence (`<table>_<column>_seq`),
            // force NOT NULL, and default to nextval() — like PostgreSQL.
            "serial" => Ok(ColType::Int),
            // v0.65: smallserial/bigserial complete the pseudo-type
            // family (real backing sequences are created for all three
            // in column definitions; casts keep the plain mapping).
            "smallserial" => Ok(ColType::SmallInt),
            "bigserial" => Ok(ColType::BigInt),
            // v0.35: character types carry their typmod, like PG19
            // (anychar_typmodin): a single positive length; anything else
            // is 22023. `varchar` without a length is unlimited (None);
            // bare `char` means `char(1)`.
            "varchar" => {
                let n = self.parse_opt_typmod()?;
                Ok(ColType::Varchar(n))
            }
            // v0.75: `information_schema.sql_identifier` is a domain over
            // varchar; for casts we treat it as varchar.
            "sql_identifier" => {
                let n = self.parse_opt_typmod()?;
                Ok(ColType::Varchar(n))
            }
            "char" => {
                let n = self.parse_opt_typmod()?;
                Ok(ColType::Char(n.or(Some(1))))
            }
            // v0.35: internal `bpchar` name; bare means typmod -1 (no
            // padding, no limit), like PG.
            "bpchar" => {
                let n = self.parse_opt_typmod()?;
                Ok(ColType::Char(n))
            }
            "character" => {
                // `character varying(n)` or plain `character(n)`.
                let varying = self.eat_keyword("varying");
                let n = self.parse_opt_typmod()?;
                if varying {
                    Ok(ColType::Varchar(n))
                } else {
                    Ok(ColType::Char(n.or(Some(1))))
                }
            }
            // v0.57: `name` (PostgreSQL internal identifier type, OID 19)
            // is its own type now: values truncate at 63 bytes on input
            // (PG19 namein) instead of behaving like text.
            "name" => Ok(ColType::Name),
            "real" | "float4" => Ok(ColType::Float4),
            "float8" => Ok(ColType::Float),
            // v0.1-v0.6 spelled the float8 column type "float"/"double".
            "float" | "double" => {
                self.eat_keyword("precision");
                Ok(ColType::Float)
            }
            "numeric" | "decimal" => {
                // v0.60: optional (p[, s]) typmod, stored on the type
                // like PG19's numerictypmodin: precision 1..1000, scale
                // -1000..1000 (22023 otherwise). Negative scales are
                // allowed since PG15 (e.g. numeric(3,-6)).
                let mut typmod: Option<(u32, i32)> = None;
                if *self.peek() == Token::LParen {
                    self.next();
                    let precision: i64 = match self.next() {
                        Token::Number(n) => n.parse().map_err(|_| {
                            err(format!("syntax error: bad numeric precision {:?}", n))
                        })?,
                        other => {
                            return Err(err(format!(
                                "syntax error: expected numeric precision, found {:?}",
                                other
                            )));
                        }
                    };
                    if precision < 1 || precision > 1000 {
                        return Err(err_typmod(format!(
                            "precision {} must be between 1 and 1000",
                            precision
                        )));
                    }
                    let mut scale: i64 = 0;
                    if *self.peek() == Token::Comma {
                        self.next();
                        let neg = if *self.peek() == Token::Minus {
                            self.next();
                            true
                        } else {
                            false
                        };
                        let mag: i64 = match self.next() {
                            Token::Number(n) => n.parse().map_err(|_| {
                                err(format!("syntax error: bad numeric scale {:?}", n))
                            })?,
                            other => {
                                return Err(err(format!(
                                    "syntax error: expected numeric scale, found {:?}",
                                    other
                                )));
                            }
                        };
                        scale = if neg { -mag } else { mag };
                        if scale < -1000 || scale > 1000 {
                            return Err(err_typmod(format!(
                                "scale {} must be between -1000 and 1000",
                                scale
                            )));
                        }
                    }
                    self.expect(Token::RParen, "')'")?;
                    typmod = Some((precision as u32, scale as i32));
                }
                Ok(ColType::Numeric(typmod))
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
            "regclass" => Ok(ColType::Regclass),
            "pg_lsn" => Ok(ColType::PgLsn), // v0.64
            // v0.81: not a builtin — treat as a (possibly) named composite
            // type; the name is resolved against the type catalog at
            // execution time (42704 if undefined).
            _ => Ok(ColType::Composite),
        }?;
        // v0.81: the composite name, if this was not a builtin.
        let composite_name = if base == ColType::Composite {
            Some(name)
        } else {
            None
        };
        // v0.79: array type suffixes — `int[]`, `int[3]`, `int[][]`
        // (PG19 `opt_array_bounds`; declared bounds are accepted and
        // ignored, exactly like PG). Each `[]` level flattens to the
        // scalar element type (ArrayElem::of).
        let mut ty = base;
        while *self.peek() == Token::LBracket {
            self.next(); // consume '['
            // An optional bound `[n]` (a plain number; anything else is
            // a syntax error, like PG).
            if let Token::Number(_) = self.peek() {
                self.next();
            }
            self.expect(Token::RBracket, "']'")?;
            ty = ColType::Array(crate::storage::ArrayElem::of(&ty));
        }
        // v0.81: `t_rec[]` — an array of composites is not supported yet;
        // keep the composite marker (the execution-time resolver will
        // report it cleanly).
        Ok((ty, composite_name))
    }

    /// v0.35: parse an optional character-type length modifier `(n)`,
    /// returning `None` when absent. Mirrors PG19's `anychar_typmodin`:
    /// exactly one positive length is required; anything else (missing
    /// digits, a second modifier, zero, negative, or over 10,485,760) is
    /// SQLSTATE 22023.
    fn parse_opt_typmod(&mut self) -> Result<Option<i32>, SqlError> {
        if *self.peek() != Token::LParen {
            return Ok(None);
        }
        self.next(); // '('
        let bad = || err_typmod("invalid type modifier".to_string());
        let n: i64 = match self.next() {
            Token::Number(s) => s.parse().map_err(|_| bad())?,
            _ => return Err(bad()),
        };
        match self.next() {
            Token::RParen => {}
            _ => return Err(bad()),
        }
        if !(1..=10_485_760).contains(&n) {
            return Err(bad());
        }
        Ok(Some(n as i32))
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
                | "smallserial"
                | "bigserial"
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
                | "regclass"
                | "pg_lsn" // v0.64
        )
    }

    fn parse_create(&mut self) -> Result<Stmt, SqlError> {
        // v0.11: CREATE ROLE / USER / GROUP
        if matches!(self.peek(), Token::Ident(s) if s == "role" || s == "user" || s == "group") {
            return self.parse_create_role();
        }
        // v0.86: CREATE [OR REPLACE] FUNCTION and CREATE OPERATOR.
        // OR REPLACE is consumed here (parse_create_function expects
        // to start at FUNCTION).
        if matches!(self.peek(), Token::Ident(s) if s == "or") {
            // Peek past `or` for `replace` without consuming on mismatch.
            let is_or_replace = matches!(self.peek2(), Token::Ident(s) if s == "replace");
            if is_or_replace {
                self.next(); // 'or'
                self.next(); // 'replace'
                if matches!(self.peek(), Token::Ident(s) if s == "function") {
                    return self.parse_create_function(true);
                }
                return Err(err(
                    "syntax error: OR REPLACE is only supported for CREATE FUNCTION".to_string(),
                ));
            }
        }
        if matches!(self.peek(), Token::Ident(s) if s == "function") {
            return self.parse_create_function(false);
        }
        if matches!(self.peek(), Token::Ident(s) if s == "operator") {
            return self.parse_create_operator();
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
            let name = if matches!(self.peek(), Token::Ident(s) if s == "on") {
                // v0.14: the index name may be omitted
                // (`CREATE INDEX ON t (a, b)`); PostgreSQL auto-names it
                // <table>_<columns>_idx. Filled in after parsing columns.
                String::new()
            } else {
                self.expect_ident()?
            };
            self.expect_keyword("on")?;
            let table = self.expect_ident()?;
            // v0.88: `USING btree` may appear between the table and the
            // column list. v0.89: rustgres only implements btree, so any
            // other access method is 0A000 (feature not supported) rather
            // than silently misbehaving as a btree.
            if self.eat_keyword("using") {
                let method = self.expect_ident()?;
                if method != "btree" {
                    return Err(SqlError {
                        message: format!("index access method \"{method}\" is not supported"),
                        code: "0A000",
                    });
                }
            }
            self.expect(Token::LParen, "'('")?;
            let mut columns = Vec::new();
            loop {
                // v0.88: `(expr)` is an index expression; anything else
                // must be a plain column name.
                let (name_opt, expr_opt) = if matches!(self.peek(), Token::LParen) {
                    self.next(); // consume the expression's '('
                    let start = self.pos;
                    let _ = self.parse_or()?;
                    let src = tokens_to_sql(&self.tokens[start..self.pos]);
                    self.expect(Token::RParen, "')'")?;
                    (None, Some(src))
                } else {
                    (Some(self.expect_ident()?), None)
                };
                let desc = if self.eat_keyword("desc") {
                    true
                } else {
                    self.eat_keyword("asc");
                    false
                };
                let nulls_first = if self.eat_keyword("nulls") {
                    if self.eat_keyword("first") {
                        true
                    } else {
                        self.expect_keyword("last")?;
                        false
                    }
                } else {
                    // PG defaults: ASC → NULLS LAST, DESC → NULLS FIRST.
                    desc
                };
                columns.push(IndexColSpec {
                    name: name_opt,
                    expr: expr_opt,
                    desc,
                    nulls_first,
                });
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
            // v0.88: partial index predicate.
            let predicate = if self.eat_keyword("where") {
                let start = self.pos;
                let _ = self.parse_or()?;
                Some(tokens_to_sql(&self.tokens[start..self.pos]))
            } else {
                None
            };
            let name = if name.is_empty() {
                let key_part: Vec<String> = columns
                    .iter()
                    .map(|c| {
                        c.name.clone().unwrap_or_else(|| {
                            // PG mangles expression auto-names; "expr" is
                            // unambiguous and cannot collide with a real
                            // column list's mangling.
                            "expr".to_string()
                        })
                    })
                    .collect();
                format!("{}_{}_idx", table, key_part.join("_"))
            } else {
                name
            };
            return Ok(Stmt::CreateIndex {
                name,
                table,
                columns,
                unique,
                if_not_exists,
                predicate,
            });
        }
        // v0.9: CREATE SEQUENCE (CREATE VIEW is intercepted before
        // tokenizing so the raw query text survives).
        if matches!(self.peek(), Token::Ident(s) if s == "sequence") {
            return self.parse_create_sequence();
        }
        // v0.75: CREATE [TEMP|TEMPORARY] SEQUENCE — accept (and ignore)
        // the temp modifier; sequences are session-agnostic here.
        if matches!(self.peek(), Token::Ident(s) if s == "temporary" || s == "temp")
            && matches!(self.peek2(), Token::Ident(s) if s == "sequence")
        {
            self.next(); // temp / temporary
            return self.parse_create_sequence();
        }
        // v0.22: CREATE TYPE (bounded): the bare shell form and the
        // parenthesized completion form with LIKE = <base>.
        if matches!(self.peek(), Token::Ident(s) if s == "type") {
            return self.parse_create_type();
        }
        // v0.85: CREATE DOMAIN name AS type [constraints...].
        if matches!(self.peek(), Token::Ident(s) if s == "domain") {
            return self.parse_create_domain();
        }
        // v0.75: CREATE STATISTICS [IF NOT EXISTS] name [(kinds)] ON
        // cols FROM table. Accepted as a no-op (statistics are not used
        // by the planner); the syntax is validated.
        if matches!(self.peek(), Token::Ident(s) if s == "statistics") {
            self.next(); // 'statistics'
            if self.eat_keyword("if") {
                self.expect_keyword("not")?;
                self.expect_keyword("exists")?;
            }
            let _name = self.expect_ident()?;
            // Optional `(dependencies)` / `(ndistinct)` / etc.
            if *self.peek() == Token::LParen {
                self.next();
                let mut depth = 1;
                while depth > 0 {
                    match self.next() {
                        Token::LParen => depth += 1,
                        Token::RParen => depth -= 1,
                        Token::EOF => {
                            return Err(err(
                                "syntax error: unterminated CREATE STATISTICS".to_string()
                            ));
                        }
                        _ => {}
                    }
                }
            }
            self.expect_keyword("on")?;
            // Column list: either `(a, b)` or bare `a, b`.
            if *self.peek() == Token::LParen {
                self.next();
                let mut cdepth = 1;
                while cdepth > 0 {
                    match self.next() {
                        Token::LParen => cdepth += 1,
                        Token::RParen => cdepth -= 1,
                        Token::EOF => {
                            return Err(err(
                                "syntax error: unterminated CREATE STATISTICS".to_string()
                            ));
                        }
                        _ => {}
                    }
                }
            } else {
                // Bare column list: `a, b, ...` until `from`.
                loop {
                    self.expect_ident()?;
                    if matches!(self.peek(), Token::Ident(s) if s == "from") {
                        break;
                    }
                    self.expect(Token::Comma, "','")?;
                }
            }
            self.expect_keyword("from")?;
            let _table = self.expect_ident()?;
            return Ok(Stmt::CreateStatistics);
        }
        // v0.14: CREATE [ { TEMPORARY | TEMP } | { GLOBAL | LOCAL } ] TABLE.
        // v0.21: TEMP tables drop any existing table with the same name
        // (approximating session-local semantics for pg_regress).
        let temp = if self.eat_keyword("temporary") || self.eat_keyword("temp") {
            true
        } else {
            // GLOBAL/LOCAL are noise words in PostgreSQL (SQL standard
            // scoping); accept and ignore them as well.
            let _ = self.eat_keyword("global") || self.eat_keyword("local");
            let _ = self.eat_keyword("temporary") || self.eat_keyword("temp");
            false
        };
        self.expect_keyword("table")?;
        // v0.48: CTAS accepts IF NOT EXISTS (PG19 CreateStmt).
        let if_not_exists = if self.eat_keyword("if") {
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        // v0.69: `CREATE TABLE name PARTITION OF parent ...` (no column list).
        if matches!(self.peek(), Token::Ident(s) if s == "partition") {
            // Distinguish `PARTITION OF` from `PARTITION BY`: only OF
            // appears directly after the table name.
            let save = self.pos;
            self.next(); // 'partition'
            let is_of = matches!(self.peek(), Token::Ident(s) if s == "of");
            self.pos = save;
            if is_of {
                return self.parse_create_partition_of(name, temp);
            }
        }
        // v0.48: `CREATE TABLE name AS <query>` (CTAS) versus the
        // column-definition list. A parenthesized group after the name
        // is the CTAS column-*alias* list only when `AS` follows its
        // closing paren (Postgres makes the same grammatical
        // distinction); otherwise it is the column definitions.
        let as_consumed = self.eat_keyword("as");
        if as_consumed || self.ctas_aliases_ahead() {
            return self.parse_create_table_as(name, temp, if_not_exists, as_consumed);
        }
        self.expect(Token::LParen, "'('")?;
        let mut items: Vec<TableItem> = Vec::new();
        // v0.74: PostgreSQL allows zero-column tables (`CREATE TABLE t()`).
        if *self.peek() == Token::RParen {
            self.next();
        } else {
            loop {
                // v0.72: `LIKE source_table [like_option ...]` — a
                // first-class item of the column list (PG19).
                if matches!(self.peek(), Token::Ident(s) if s == "like") {
                    items.push(TableItem::Like(self.parse_like_clause()?));
                } else if self.is_table_constraint_start() {
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
        } // v0.74: end of non-empty column list; `()` handled above.
        let mut def = build_table_def(&name, items)?;
        // v0.77: `INHERITS (parent [, ...])` — table inheritance. The
        // child copies the parents' columns at exec; recorded in the
        // def for future inheritance-expansion support.
        if self.eat_keyword("inherits") {
            self.expect(Token::LParen, "'('")?;
            loop {
                def.inherits.push(self.expect_ident()?);
                match self.next() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')' in INHERITS list, found {:?}",
                            other
                        )));
                    }
                }
            }
        }
        // v0.72: `WITH (storage_parameter = value, ...)` (PG19
        // reloptions). Parsed into the def; exec validates the
        // recognized parameters (fillfactor range) and records them.
        if self.eat_keyword("with") {
            def.reloptions = self.parse_reloptions()?;
        }
        // v0.69: `PARTITION BY ...` after the column list.
        if matches!(self.peek(), Token::Ident(s) if s == "partition") {
            let (method, keys) = self.parse_partition_by()?;
            def.partition = Some(PartitionDef {
                method,
                keys,
                parent: None,
                bound: None,
            });
        }
        Ok(Stmt::CreateTable { name, def, temp })
    }

    /// v0.69: `CREATE TABLE name PARTITION OF parent FOR VALUES ...`
    /// (no column list). Called from `parse_create` when `PARTITION`
    /// follows the table name.
    fn parse_create_partition_of(&mut self, name: String, temp: bool) -> Result<Stmt, SqlError> {
        let (parent, bound, sub) = self.parse_partition_of()?;
        // The child inherits the parent's method and key; a trailing
        // `PARTITION BY` makes it a sub-partitioned intermediate.
        let (method, keys) = match sub {
            Some((m, k)) => (m, k),
            None => {
                // Placeholder: exec resolves the parent's method/key.
                // We mark it via a sentinel — exec will fill it in.
                // (Parser doesn't have catalog access.)
                (crate::storage::PartMethod::List, Vec::new())
            }
        };
        let def = TableDef {
            columns: Vec::new(), // inherited from parent at exec
            composite_types: Vec::new(),
            domain_types: Vec::new(),
            domain_elem: Vec::new(),
            not_null: Vec::new(),
            defaults: Vec::new(),
            serial: Vec::new(),
            compression: Vec::new(),
            storage: Vec::new(),
            checks: Vec::new(),
            uniques: Vec::new(),
            pkey: None,
            fks: Vec::new(),
            partition: Some(PartitionDef {
                method,
                keys,
                parent: Some(parent),
                bound: Some(bound),
            }),
            likes: Vec::new(),
            reloptions: Vec::new(),
            inherits: Vec::new(),
        };
        Ok(Stmt::CreateTable { name, def, temp })
    }

    /// v0.48: true when the tokens ahead are `( ... ) AS` — the CTAS
    /// column-alias list — rather than a column-definition list. Scans
    /// to the matching close paren (depth-counted) and checks the token
    /// after it, without consuming anything.
    fn ctas_aliases_ahead(&self) -> bool {
        if *self.peek() != Token::LParen {
            return false;
        }
        let mut depth = 0usize;
        let mut k = 0usize;
        loop {
            match self.tokens.get(self.pos + k) {
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(self.pos + k + 1),
                            Some(Token::Ident(s)) if s == "as"
                        );
                    }
                }
                None => return false,
                _ => {}
            }
            k += 1;
            // Defensive bound: a real column list never scans this far.
            if k > 4096 {
                return false;
            }
        }
    }

    /// v0.48: `CREATE [TEMP] TABLE [IF NOT EXISTS] name [(alias, ...)]
    /// AS <query> [WITH [NO] DATA]` (PG19 createas.c). `as_consumed`
    /// tells whether `parse_create` already ate the `AS` keyword (when
    /// it did, a following paren group starts the query, not aliases).
    fn parse_create_table_as(
        &mut self,
        name: String,
        temp: bool,
        if_not_exists: bool,
        as_consumed: bool,
    ) -> Result<Stmt, SqlError> {
        // Optional column aliases rename the query's output columns
        // positionally (PG19 SelectInto).
        let col_aliases = if !as_consumed && *self.peek() == Token::LParen {
            self.next(); // '('
            let mut aliases = Vec::new();
            loop {
                aliases.push(self.expect_ident()?);
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
            self.expect_keyword("as")?;
            aliases
        } else {
            Vec::new()
        };
        // The query: SELECT, WITH...SELECT, or a parenthesized query.
        // (TABLE / VALUES query forms are not supported in this version.)
        // Note: when `as_consumed` is false the only leading paren group
        // was the alias list, already consumed above — so any `(` here
        // starts a parenthesized query.
        let select = if *self.peek() == Token::LParen {
            self.next(); // '('
            let q = self.parse_select_query_not_consumed()?;
            self.expect(Token::RParen, "')'")?;
            q
        } else if matches!(self.peek(), Token::Ident(s) if s == "with") {
            match self.parse_with()? {
                Stmt::Select(sel) => sel,
                _ => {
                    return Err(err(
                        "syntax error: CREATE TABLE AS requires a SELECT query".to_string()
                    ));
                }
            }
        } else {
            self.expect_keyword("select")?;
            self.parse_select_query()?
        };
        // Optional trailing `WITH [NO] DATA` (default: WITH DATA).
        let with_data = if self.eat_keyword("with") {
            let no_data = self.eat_keyword("no");
            self.expect_keyword("data")?;
            !no_data
        } else {
            true
        };
        Ok(Stmt::CreateTableAs {
            name,
            col_aliases,
            select: Box::new(select),
            temp,
            if_not_exists,
            with_data,
        })
    }

    /// True when the next tokens start a table-level constraint rather
    /// than a column definition.
    fn is_table_constraint_start(&mut self) -> bool {
        matches!(self.peek(), Token::Ident(s)
            if s == "constraint" || s == "primary" || s == "unique"
                || s == "check" || s == "foreign")
    }

    /// v0.72: `LIKE source_table [INCLUDING|EXCLUDING kind ...]`
    /// (PG19 transformTableLikeClause). The source name resolves at
    /// exec, which has catalog access.
    fn parse_like_clause(&mut self) -> Result<LikeClause, SqlError> {
        self.expect_keyword("like")?;
        // v0.72: the source may be schema-qualified (PG19).
        let mut source = self.expect_ident()?;
        while matches!(self.peek(), Token::Dot) {
            self.next(); // '.'
            source.push('.');
            source.push_str(&self.expect_ident()?);
        }
        let mut options = Vec::new();
        loop {
            let including = if self.eat_keyword("including") {
                true
            } else if self.eat_keyword("excluding") {
                false
            } else {
                break;
            };
            let kind = if self.eat_keyword("all") {
                LikeKind::All
            } else if self.eat_keyword("comments") {
                LikeKind::Comments
            } else if self.eat_keyword("compression") {
                LikeKind::Compression
            } else if self.eat_keyword("constraints") {
                LikeKind::Constraints
            } else if self.eat_keyword("defaults") {
                LikeKind::Defaults
            } else if self.eat_keyword("identity") {
                LikeKind::Identity
            } else if self.eat_keyword("indexes") {
                LikeKind::Indexes
            } else if self.eat_keyword("statistics") {
                LikeKind::Statistics
            } else if self.eat_keyword("storage") {
                LikeKind::Storage
            } else if self.eat_keyword("generated") {
                LikeKind::Generated
            } else {
                return Err(err(format!(
                    "syntax error: unrecognized LIKE option near {:?}",
                    self.peek()
                )));
            };
            options.push(LikeOption { including, kind });
        }
        Ok(LikeClause { source, options })
    }

    /// `name type [column constraints...]`.
    fn parse_column_def(&mut self) -> Result<ParsedColDef, SqlError> {
        let name = self.expect_ident()?;
        // v0.65: detect the serial pseudo-types from the raw type name
        // before it resolves to a plain ColType (the marker drives
        // backing-sequence creation in exec).
        let serial = match self.peek() {
            Token::Ident(s) | Token::QIdent(s) => match s.as_str() {
                "serial" => Some(SerialKind::Serial),
                "smallserial" => Some(SerialKind::SmallSerial),
                "bigserial" => Some(SerialKind::BigSerial),
                _ => None,
            },
            _ => None,
        };
        let (col_type, composite_name) = self.parse_col_type()?;
        // v0.41: PG19 `opt_column_compression` sits between the type
        // name and the column constraints.
        let compression = if self.eat_keyword("compression") {
            Some(self.expect_ident()?)
        } else {
            None
        };
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
            composite_name,
            serial,
            compression,
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
        } else if self.eat_keyword("not") {
            // v0.76: `NOT NULL col [NOT VALID]`.
            self.expect_keyword("null")?;
            let col = self.expect_ident()?;
            let not_valid = if self.eat_keyword("not") {
                self.expect_keyword("valid")?;
                true
            } else {
                // Plain `NOT VALID` without NOT is not valid syntax here,
                // but accept a bare `VALID` as a no-op for robustness.
                let _ = self.eat_keyword("valid");
                false
            };
            Ok(ParsedTableCon::NotNull {
                name: cname,
                col,
                not_valid,
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
        if *self.peek() == Token::LParen {
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

    /// ALTER TABLE name <action> [, <action> ...] (v0.72: PG19 takes a
    /// comma-separated action list; each action parses exactly as the
    /// single-action form did, so commas inside parenthesized option
    /// lists are unaffected).
    fn parse_alter(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("table")?;
        let name = self.expect_ident()?;
        let mut actions = Vec::new();
        loop {
            actions.push(self.parse_alter_action()?);
            if *self.peek() == Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        Ok(Stmt::AlterTable { name, actions })
    }

    // ------------------------------------------------------------------
    // v0.69: declarative partitioning (PG19 partdef.c).
    // ------------------------------------------------------------------

    /// Parse `PARTITION BY RANGE|LIST|HASH (key [, ...])`. Each key is a
    /// column name, a parenthesized expression, or a column with an
    /// (ignored) operator class name.
    fn parse_partition_by(
        &mut self,
    ) -> Result<(crate::storage::PartMethod, Vec<PartitionKeyDef>), SqlError> {
        use crate::storage::PartMethod;
        self.expect_keyword("partition")?;
        self.expect_keyword("by")?;
        let method = if self.eat_keyword("range") {
            PartMethod::Range
        } else if self.eat_keyword("list") {
            PartMethod::List
        } else if self.eat_keyword("hash") {
            PartMethod::Hash
        } else {
            return Err(err(format!(
                "syntax error: expected RANGE, LIST or HASH after PARTITION BY, found {:?}",
                self.peek()
            )));
        };
        self.expect(Token::LParen, "'('")?;
        let mut keys = Vec::new();
        loop {
            // v0.70: PG accepts any expression as a partition key, e.g.
            // `PARTITION BY LIST (lower(a))` — not just parenthesized ones
            // or bare columns. A bare (optionally opclass-qualified) column
            // stays a column key; anything else becomes an expression key.
            let e = self.parse_or()?;
            match e {
                Expr::Column { table: None, name } => {
                    // Optional operator class (e.g. `a part_test_int4_ops`):
                    // parsed and ignored (PG19 uses it for hash opclasses;
                    // our hash uses the value directly).
                    if matches!(self.peek(), Token::Ident(s) if s != "collate") {
                        // Peek: is it followed by ',' or ')'? If so it's an
                        // opclass name, not the next key. We can't easily
                        // lookahead two tokens, so consume it only if the
                        // next token after it is ',' or ')'.
                        let save = self.pos;
                        let _opclass = self.expect_ident()?;
                        match self.peek() {
                            Token::Comma | Token::RParen => {}
                            _ => {
                                // Not an opclass — rewind (it was the next key,
                                // but keys are comma-separated so this means
                                // a syntax error; let the ','/' )' check fail).
                                self.pos = save;
                            }
                        }
                    }
                    keys.push(PartitionKeyDef::Column(name));
                }
                other => {
                    keys.push(PartitionKeyDef::Expr(other));
                }
            }
            match self.next() {
                Token::Comma => continue,
                Token::RParen => break,
                other => {
                    return Err(err(format!(
                        "syntax error: expected ',' or ')' in PARTITION BY, found {:?}",
                        other
                    )));
                }
            }
        }
        if keys.is_empty() {
            return Err(err(
                "syntax error: PARTITION BY requires at least one key".to_string()
            ));
        }
        Ok((method, keys))
    }

    /// Parse `FOR VALUES IN (...) | FROM (..) TO (..) | WITH (...) |
    /// DEFAULT` (the `DEFAULT` keyword alone, without FOR VALUES).
    fn parse_partition_bound(&mut self) -> Result<PartBoundDef, SqlError> {
        if self.eat_keyword("default") {
            return Ok(PartBoundDef::Default);
        }
        self.expect_keyword("for")?;
        self.expect_keyword("values")?;
        if self.eat_keyword("in") {
            self.expect(Token::LParen, "'('")?;
            let mut vals = Vec::new();
            loop {
                // NULL is allowed in the list (means "null partition").
                if matches!(self.peek(), Token::Ident(s) if s == "null") {
                    self.next();
                    vals.push(Expr::Literal(Literal::Null));
                } else {
                    vals.push(self.parse_or()?);
                }
                match self.next() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')' in FOR VALUES IN, found {:?}",
                            other
                        )));
                    }
                }
            }
            return Ok(PartBoundDef::List(vals));
        }
        if self.eat_keyword("from") {
            let lower = self.parse_range_bound_list()?;
            self.expect_keyword("to")?;
            let upper = self.parse_range_bound_list()?;
            return Ok(PartBoundDef::Range { lower, upper });
        }
        if self.eat_keyword("with") {
            self.expect(Token::LParen, "'('")?;
            self.expect_keyword("modulus")?;
            let modulus = self.expect_u32()?;
            self.expect(Token::Comma, "','")?;
            self.expect_keyword("remainder")?;
            let remainder = self.expect_u32()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(PartBoundDef::Hash { modulus, remainder });
        }
        Err(err(format!(
            "syntax error: expected IN, FROM, WITH or DEFAULT after FOR VALUES, found {:?}",
            self.peek()
        )))
    }

    /// Parse `(MINVALUE | MAXVALUE | expr [, ...])` for a RANGE endpoint.
    fn parse_range_bound_list(&mut self) -> Result<Vec<RangeBoundDef>, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let mut out = Vec::new();
        loop {
            if self.eat_keyword("minvalue") {
                out.push(RangeBoundDef::Min);
            } else if self.eat_keyword("maxvalue") {
                out.push(RangeBoundDef::Max);
            } else {
                out.push(RangeBoundDef::Val(self.parse_or()?));
            }
            match self.next() {
                Token::Comma => continue,
                Token::RParen => break,
                other => {
                    return Err(err(format!(
                        "syntax error: expected ',' or ')' in range bound, found {:?}",
                        other
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Parse `PARTITION OF parent FOR VALUES ... [PARTITION BY ...]`.
    fn parse_partition_of(
        &mut self,
    ) -> Result<
        (
            String,
            PartBoundDef,
            Option<(crate::storage::PartMethod, Vec<PartitionKeyDef>)>,
        ),
        SqlError,
    > {
        self.expect_keyword("partition")?;
        self.expect_keyword("of")?;
        let parent = self.expect_ident()?;
        let bound = self.parse_partition_bound()?;
        // Optional sub-partitioning: `PARTITION BY ...`.
        let sub = if matches!(self.peek(), Token::Ident(s) if s == "partition") {
            Some(self.parse_partition_by()?)
        } else {
            None
        };
        Ok((parent, bound, sub))
    }

    fn parse_alter_action(&mut self) -> Result<AlterAction, SqlError> {
        // v0.69: ALTER TABLE parent ATTACH PARTITION child FOR VALUES ...
        if self.eat_keyword("attach") {
            self.expect_keyword("partition")?;
            let child = self.expect_ident()?;
            let bound = self.parse_partition_bound()?;
            return Ok(AlterAction::AttachPartition { child, bound });
        }
        // v0.11: ALTER TABLE name OWNER TO role
        if self.eat_keyword("owner") {
            self.expect_keyword("to")?;
            return Ok(AlterAction::OwnerTo {
                new_owner: self.expect_ident()?,
            });
        }
        // v0.37: ALTER TABLE name SET (opt = val, ...) — reloptions.
        if self.eat_keyword("set") {
            let options = self.parse_reloptions()?;
            return Ok(AlterAction::SetRelOptions { options });
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
            // v0.65: serial implies NOT NULL (an explicit NULL conflicts,
            // like PG's 42601).
            if col.serial.is_some() {
                not_null = true;
            }
            for con in col.cons {
                match con {
                    ColCon::NotNull => not_null = true,
                    ColCon::Null => {
                        if col.serial.is_some() {
                            return Err(err(format!(
                                "conflicting NULL/NOT NULL declarations for column \"{}\"",
                                col.name
                            )));
                        }
                        not_null = false
                    }
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
                        name: n
                            .unwrap_or_else(|| format!("{}_check_{}", col.name, checks.len() + 1)),
                        expr: e,
                        not_valid: false,
                        kind: CheckKind::Check,
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
                serial: col.serial,
                compression: col.compression,
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
                // v0.37: ALTER COLUMN c SET STORAGE mode
                if self.eat_keyword("storage") {
                    let mode = self.expect_ident()?;
                    return Ok(AlterAction::SetStorage {
                        column: cname,
                        mode,
                    });
                }
                // v0.41: ALTER COLUMN c SET COMPRESSION method
                // (PG19 `AT_SetCompression`).
                if self.eat_keyword("compression") {
                    let mode = self.expect_ident()?;
                    return Ok(AlterAction::SetCompression {
                        column: cname,
                        mode,
                    });
                }
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
                notnull: None,
            }),
            ParsedTableCon::Unique(name, cols) => Ok(AlterAction::AddConstraint {
                unique: Some(UniqueDef {
                    name: name.unwrap_or_default(),
                    cols,
                }),
                check: None,
                pkey: None,
                fk: None,
                notnull: None,
            }),
            ParsedTableCon::Check(name, e) => Ok(AlterAction::AddConstraint {
                check: Some(CheckDef {
                    name: name.unwrap_or_default(),
                    expr: e,
                    not_valid: false,
                    kind: CheckKind::Check,
                }),
                unique: None,
                pkey: None,
                fk: None,
                notnull: None,
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
                notnull: None,
            }),
            ParsedTableCon::NotNull {
                name,
                col,
                not_valid,
            } => Ok(AlterAction::AddConstraint {
                notnull: Some(NotNullDef {
                    name: name.unwrap_or_default(),
                    col,
                    not_valid,
                }),
                check: None,
                unique: None,
                pkey: None,
                fk: None,
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

    /// v0.22: bounded CREATE TYPE. Two forms:
    /// - `CREATE TYPE name;` — registers a shell type.
    /// - `CREATE TYPE name (attr = value, ...);` — completes the type;
    ///   only LIKE = <base> is interpreted, the remaining attributes
    ///   (input/output functions, etc.) are accepted and ignored.
    fn parse_create_type(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("type")?;
        let name = self.expect_ident()?;
        // v0.81: `CREATE TYPE name AS (field type, ...)` — PG19 named
        // composite type. The fields are parsed like table columns
        // (type names may themselves be composites).
        if self.eat_keyword("as") {
            self.expect(Token::LParen, "'('")?;
            let mut fields = Vec::new();
            loop {
                if *self.peek() == Token::RParen {
                    self.next();
                    break;
                }
                let fname = self.expect_ident()?;
                let (fty, composite_name) = self.parse_type_name()?;
                fields.push((fname, fty, composite_name));
                match self.next() {
                    Token::Comma => {}
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')', found {:?}",
                            other
                        )));
                    }
                }
            }
            return Ok(Stmt::CreateType {
                name,
                like_base: None,
                composite: Some(fields),
            });
        }
        let like_base = if *self.peek() == Token::LParen {
            self.next();
            let mut like_base = None;
            loop {
                let key = self.expect_ident()?;
                self.expect(Token::Eq, "'='")?;
                let val = self.expect_ident()?;
                if key == "like" {
                    like_base = Some(val);
                }
                match self.next() {
                    Token::Comma => {}
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')', found {:?}",
                            other
                        )));
                    }
                }
            }
            like_base
        } else {
            None
        };
        Ok(Stmt::CreateType {
            name,
            like_base,
            composite: None,
        })
    }

    /// v0.86: `CREATE [OR REPLACE] FUNCTION name ([argname] type
    /// [, ...]) RETURNS [SETOF] type [LANGUAGE lang] [IMMUTABLE |
    /// STABLE | VOLATILE] [STRICT] AS 'body'` — PG19 CreateFunctionStmt,
    /// bounded to `LANGUAGE sql` and `LANGUAGE internal`. Any other
    /// language is rejected here with 42601 (honest unsupported).
    /// Argument types are stored as written and resolved at execution;
    /// the body string is parsed once at execution time.
    fn parse_create_function(&mut self, or_replace: bool) -> Result<Stmt, SqlError> {
        self.expect_keyword("function")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "'('")?;
        let mut args = Vec::new();
        if *self.peek() != Token::RParen {
            loop {
                args.push(self.parse_func_arg()?);
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
        } else {
            self.next(); // ')'
        }
        self.expect_keyword("returns")?;
        let returns_set = self.eat_keyword("setof");
        let ret_type = self.parse_func_type_name()?;
        let mut lang: Option<FuncLang> = None;
        let mut body: Option<String> = None;
        let mut volatility = FuncVolatility::Volatile;
        let mut strict = false;
        loop {
            if self.eat_keyword("language") {
                let lname = self.expect_ident()?;
                lang = Some(match lname.as_str() {
                    "sql" => FuncLang::Sql,
                    "internal" => FuncLang::Internal,
                    other => {
                        return Err(err(format!("language \"{}\" is not supported", other)));
                    }
                });
            } else if self.eat_keyword("immutable") {
                volatility = FuncVolatility::Immutable;
            } else if self.eat_keyword("stable") {
                volatility = FuncVolatility::Stable;
            } else if self.eat_keyword("volatile") {
                volatility = FuncVolatility::Volatile;
            } else if self.eat_keyword("strict") {
                strict = true;
            } else if self.eat_keyword("as") {
                match self.next() {
                    Token::Str(s) => {
                        // PG allows several AS items (body, obj file);
                        // the first is the body (for LANGUAGE internal,
                        // the C symbol name).
                        if body.is_none() {
                            body = Some(s);
                        }
                    }
                    other => {
                        return Err(err(format!(
                            "syntax error: expected function body string, found {:?}",
                            other
                        )));
                    }
                }
                // A second `AS '...'` (C obj file) is consumed the same
                // way on the next loop iteration.
                if self.eat_keyword("as") {
                    match self.next() {
                        Token::Str(_) => {}
                        other => {
                            return Err(err(format!(
                                "syntax error: expected function body string, found {:?}",
                                other
                            )));
                        }
                    }
                }
            } else {
                break;
            }
        }
        let lang = lang.unwrap_or(FuncLang::Sql);
        let body = body.ok_or_else(|| err("syntax error: expected AS 'body'".to_string()))?;
        Ok(Stmt::CreateFunction {
            name,
            args,
            ret_type,
            returns_set,
            lang,
            body,
            or_replace,
            volatility,
            strict,
        })
    }

    /// v0.86: one function argument: `[name] type`. A lone identifier
    /// followed by `,` or `)` is the type; otherwise the first
    /// identifier is the argument name. (Limitation: multi-word type
    /// names like `double precision` require an argument name, except
    /// `double precision` itself which is special-cased.)
    fn parse_func_arg(&mut self) -> Result<FuncArg, SqlError> {
        let w1 = self.expect_ident()?;
        let bare_type = matches!(self.peek(), Token::Comma | Token::RParen)
            || (w1 == "double" && matches!(self.peek(), Token::Ident(s) if s == "precision"));
        if bare_type {
            let type_name = if w1 == "double" {
                self.next(); // 'precision'
                "double precision".to_string()
            } else {
                w1
            };
            Ok(FuncArg {
                name: None,
                type_name,
            })
        } else {
            let type_name = self.parse_func_type_name()?;
            Ok(FuncArg {
                name: Some(w1),
                type_name,
            })
        }
    }

    /// v0.86: a type name as written for function signatures: an
    /// optionally schema-qualified identifier (`pg_catalog.int4`
    /// keeps `int4`). Array `[]` suffixes are rejected (bounded).
    fn parse_func_type_name(&mut self) -> Result<String, SqlError> {
        let mut name = self.expect_ident()?;
        if *self.peek() == Token::Dot && matches!(self.peek2(), Token::Ident(_)) {
            self.next(); // '.'
            name = self.expect_ident()?;
        }
        if *self.peek() == Token::LBracket {
            return Err(err(
                "array argument/return types are not supported".to_string()
            ));
        }
        Ok(name)
    }

    /// v0.86: `DROP FUNCTION [IF EXISTS] name ([type [, ...]])`.
    /// Argument names are accepted and ignored (PG allows them).
    fn parse_drop_function(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("function")?;
        let if_exists = if self.eat_keyword("if") {
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "'('")?;
        let mut arg_types = Vec::new();
        if *self.peek() != Token::RParen {
            loop {
                let arg = self.parse_func_arg()?;
                arg_types.push(arg.type_name);
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
        } else {
            self.next(); // ')'
        }
        // v0.87: optional CASCADE / RESTRICT (PG19; RESTRICT default).
        let cascade = if self.eat_keyword("cascade") {
            true
        } else {
            self.eat_keyword("restrict");
            false
        };
        Ok(Stmt::DropFunction {
            name,
            arg_types,
            if_exists,
            cascade,
        })
    }

    /// v0.86: `DROP OPERATOR [IF EXISTS] name (lefttype, righttype)`
    /// (PG19 DropOpStmt; `NONE` for a missing side).
    fn parse_drop_operator(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("operator")?;
        let if_exists = if self.eat_keyword("if") {
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.parse_operator_name()?;
        self.expect(Token::LParen, "'('")?;
        let leftarg = self.parse_op_arg_type()?;
        self.expect(Token::Comma, "','")?;
        let rightarg = self.parse_op_arg_type()?;
        self.expect(Token::RParen, "')'")?;
        // v0.87: optional CASCADE / RESTRICT (PG19; RESTRICT default).
        let cascade = if self.eat_keyword("cascade") {
            true
        } else {
            self.eat_keyword("restrict");
            false
        };
        Ok(Stmt::DropOperator {
            name,
            leftarg,
            rightarg,
            if_exists,
            cascade,
        })
    }

    /// Parse one side of a DROP OPERATOR signature: a type name or NONE.
    /// Returns the raw type name text (validation happens at execution).
    fn parse_op_arg_type(&mut self) -> Result<Option<String>, SqlError> {
        if matches!(self.peek(), Token::Ident(s) if s == "none") {
            self.next();
            return Ok(None);
        }
        let first = self.expect_ident()?;
        // Bounded multiword builtins.
        let name = match first.as_str() {
            "double" => {
                self.expect_keyword("precision")?;
                "double precision".to_string()
            }
            "character" => {
                self.expect_keyword("varying")?;
                "character varying".to_string()
            }
            _ => first,
        };
        Ok(Some(name))
    }

    /// Parse an operator name: `=` or a `?`-led `Token::Op`.
    fn parse_operator_name(&mut self) -> Result<String, SqlError> {
        match self.next() {
            Token::Eq => Ok("=".to_string()),
            Token::Op(s) => Ok(s),
            other => Err(err(format!(
                "syntax error: expected operator name, found {:?}",
                other
            ))),
        }
    }

    /// v0.86: `CREATE OPERATOR name (PROCEDURE = func [, LEFTARG =
    /// type] [, RIGHTARG = type] [, COMMUTATOR = op] [, NEGATOR = op]
    /// [, HASHES] [, MERGES])` — PG19 DefineOpStmt, bounded: the name
    /// is `=` or a `?`-led operator; only PROCEDURE/LEFTARG/RIGHTARG
    /// affect execution (IN-subquery equality), the rest are stored.
    fn parse_create_operator(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("operator")?;
        let name = self.parse_operator_name()?;
        self.expect(Token::LParen, "'('")?;
        let mut procedure: Option<String> = None;
        let mut leftarg: Option<String> = None;
        let mut rightarg: Option<String> = None;
        let mut commutator: Option<String> = None;
        let mut negator: Option<String> = None;
        let mut hashes = false;
        let mut merges = false;
        loop {
            if self.eat_keyword("hashes") {
                hashes = true;
            } else if self.eat_keyword("merges") {
                merges = true;
            } else {
                let opt = self.expect_ident()?;
                self.expect(Token::Eq, "'='")?;
                match opt.as_str() {
                    "procedure" => procedure = Some(self.expect_ident()?),
                    "leftarg" => leftarg = Some(self.parse_func_type_name()?),
                    "rightarg" => rightarg = Some(self.parse_func_type_name()?),
                    "commutator" => commutator = Some(self.parse_oper_name()?),
                    "negator" => negator = Some(self.parse_oper_name()?),
                    // v0.86: RESTRICT / JOIN selectivity estimators are
                    // parsed and ignored (bounded: no planner use).
                    "restrict" | "join" => {
                        let _ = self.expect_ident()?;
                    }
                    other => {
                        return Err(err(format!(
                            "syntax error: unknown operator option \"{}\"",
                            other
                        )));
                    }
                }
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
        let procedure =
            procedure.ok_or_else(|| err("syntax error: expected PROCEDURE".to_string()))?;
        Ok(Stmt::CreateOperator {
            name,
            procedure,
            leftarg,
            rightarg,
            commutator,
            negator,
            hashes,
            merges,
        })
    }

    /// v0.86: an operator name inside CREATE OPERATOR options
    /// (`=` or a `?`-led name).
    fn parse_oper_name(&mut self) -> Result<String, SqlError> {
        match self.next() {
            Token::Eq => Ok("=".to_string()),
            Token::Op(s) => Ok(s),
            other => Err(err(format!(
                "syntax error: expected operator name, found {:?}",
                other
            ))),
        }
    }

    /// v0.85: `CREATE DOMAIN name AS type [CONSTRAINT cname] CHECK
    /// (expr) [...] [NOT NULL | NULL] [DEFAULT expr]` — PG19
    /// CreateDomainStmt, bounded to the supported constraint kinds
    /// (CHECK / NOT NULL / DEFAULT; no UNIQUE/PKEY/FK on domains).
    fn parse_create_domain(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("domain")?;
        let name = self.expect_ident()?;
        self.expect_keyword("as")?;
        let (base, base_named) = self.parse_type_name()?;
        let mut checks = Vec::new();
        let mut not_null = false;
        let mut default = None;
        loop {
            let cname = if self.eat_keyword("constraint") {
                Some(self.expect_ident()?)
            } else {
                None
            };
            if self.eat_keyword("check") {
                self.expect(Token::LParen, "'('")?;
                let e = self.parse_or()?;
                self.expect(Token::RParen, "')'")?;
                validate_constraint_expr(&e, "CHECK")?;
                // PG19 auto-names domain checks `<domain>_check`,
                // `<domain>_check1`, ... (ChooseConstraintName).
                let cname = cname.unwrap_or_else(|| {
                    let mut n = format!("{}_check", name);
                    let mut i = 1;
                    while checks.iter().any(|c: &CheckDef| c.name == n) {
                        n = format!("{}_check{}", name, i);
                        i += 1;
                    }
                    n
                });
                checks.push(CheckDef {
                    name: cname,
                    expr: e,
                    not_valid: false,
                    kind: CheckKind::Check,
                });
            } else if self.eat_keyword("not") {
                self.expect_keyword("null")?;
                if cname.is_some() {
                    return Err(err(
                        "syntax error: CONSTRAINT name not allowed on NOT NULL".to_string()
                    ));
                }
                not_null = true;
            } else if self.eat_keyword("null") {
                if cname.is_some() {
                    return Err(err(
                        "syntax error: CONSTRAINT name not allowed on NULL".to_string()
                    ));
                }
                not_null = false;
            } else if self.eat_keyword("default") {
                if cname.is_some() {
                    return Err(err(
                        "syntax error: CONSTRAINT name not allowed on DEFAULT".to_string()
                    ));
                }
                let e = self.parse_or()?;
                validate_constraint_expr(&e, "DEFAULT")?;
                default = Some(classify_default(e)?);
            } else {
                if cname.is_some() {
                    return Err(err(
                        "syntax error: expected constraint type after CONSTRAINT name".to_string(),
                    ));
                }
                break;
            }
        }
        Ok(Stmt::CreateDomain {
            name,
            base,
            base_named,
            checks,
            not_null,
            default,
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
                // v0.28: PG 16+ 0x/0o/0b integer literals (lexed above).
                let ival: Option<i64> = if raw.len() > 2 && raw.as_bytes()[0] == b'0' {
                    match raw.as_bytes()[1] {
                        b'x' | b'X' => parse_int_radix(&raw[2..], 16),
                        b'o' | b'O' => parse_int_radix(&raw[2..], 8),
                        b'b' | b'B' => parse_int_radix(&raw[2..], 2),
                        // v0.40: PG's `{decinteger}` allows `_` between
                        // digits; strip them like `parse_int_radix` does.
                        _ => raw.replace('_', "").parse().ok(),
                    }
                } else {
                    raw.replace('_', "").parse().ok()
                };
                if let Some(i) = ival {
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
            Token::Str(s) => Ok(Literal::Text(s.into())),
            Token::UStr(raw) => {
                // v0.19: U&'...' with optional UESCAPE 'c' clause.
                let escape = self.parse_uescape()?;
                match decode_ustr(&raw, escape) {
                    Some(t) => Ok(Literal::Text(t.into())),
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
        // v0.72: `INSERT INTO t (SELECT ...)` — a parenthesized query
        // is a row source, not a column list (PG19). Peek past the
        // paren: a query starter means "no column list".
        let paren_opens_query = *self.peek() == Token::LParen
            && matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Ident(s)) if s == "select" || s == "values" || s == "table"
            );
        let columns = if *self.peek() == Token::LParen && !paren_opens_query {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.parse_insert_target()?);
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
        // v0.72: `INSERT INTO ... (SELECT ...)` — parenthesized query
        // source (PG19); parse_select_query_not_consumed eats the parens.
        // v0.74: `INSERT INTO t DEFAULT VALUES` (PG19) — one row with
        // every column at its default. Parsed as a single empty row;
        // exec's default-filling handles the rest.
        let (rows, select) = if self.eat_keyword("default") {
            self.expect_keyword("values")?;
            (vec![Vec::new()], None)
        } else if self.eat_keyword("select") {
            let sel = self.parse_select_query()?;
            (Vec::new(), Some(sel))
        } else if *self.peek() == Token::LParen {
            let sel = self.parse_select_query_not_consumed()?;
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
                if *self.peek() == Token::Comma {
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

    /// v0.84: one INSERT column-list target — a column name with
    /// optional PG19 indirection (`f2[1]`, `f3.if2`, `f4[1].if2[1]`).
    /// Adjacent `[..][..]` pairs merge into one multi-index step
    /// (PG19 gram.y); a `[l:u]` slice parses to `InsertIndirection::Slice`
    /// so execution can reject it with a PG-shaped 0A000.
    fn parse_insert_target(&mut self) -> Result<InsertTarget, SqlError> {
        let name = self.expect_ident()?;
        let mut indirection = Vec::new();
        loop {
            match self.peek() {
                Token::Dot => {
                    self.next(); // consume '.'
                    let field = self.expect_ident()?;
                    indirection.push(InsertIndirection::Field(field));
                }
                Token::LBracket => {
                    self.next(); // consume '['
                    // Slice form `[l:u]` / `[:u]` / `[l:]` — PG19
                    // `indirection_el` allows it; not assignable here.
                    let is_slice = if *self.peek() == Token::Colon {
                        self.next();
                        if *self.peek() != Token::RBracket {
                            let _ = self.parse_or()?;
                        }
                        self.expect(Token::RBracket, "']'")?;
                        true
                    } else {
                        let first = self.parse_or()?;
                        if *self.peek() == Token::Colon {
                            self.next(); // consume ':'
                            if *self.peek() != Token::RBracket {
                                let _ = self.parse_or()?;
                            }
                            self.expect(Token::RBracket, "']'")?;
                            true
                        } else {
                            self.expect(Token::RBracket, "']'")?;
                            // Merge with a preceding adjacent Index step.
                            match indirection.last_mut() {
                                Some(InsertIndirection::Index(idxs)) => {
                                    idxs.push(first);
                                }
                                _ => indirection.push(InsertIndirection::Index(vec![first])),
                            }
                            false
                        }
                    };
                    if is_slice {
                        indirection.push(InsertIndirection::Slice);
                    }
                }
                _ => break,
            }
        }
        Ok(InsertTarget { name, indirection })
    }

    /// v0.10: `RETURNING * | expr [, ...]` after INSERT/UPDATE/DELETE.
    /// Empty when the keyword is absent.
    fn parse_returning(&mut self) -> Result<Vec<SelectItem>, SqlError> {
        if !self.eat_keyword("returning") {
            return Ok(Vec::new());
        }
        let mut items = Vec::new();
        loop {
            if *self.peek() == Token::Star {
                self.next();
                items.push(SelectItem::All);
            } else if let Some(q) = self.peek_ident() {
                // v0.36: `qual.*` with a double-quoted qualifier.
                let q = q.to_string();
                if *self.peek2() == Token::Dot && *self.peek3() == Token::Star {
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
            if *self.peek() == Token::Comma {
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
        } else if *self.peek() == Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if *self.peek() == Token::Comma {
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
                    if *self.peek() == Token::Comma {
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

    /// v0.76: parse the `name [(cols)] AS [NOT MATERIALIZED] (body) [, ...]`
    /// list of a WITH clause. Returns the CTE defs and the RECURSIVE flag.
    /// Extracted from `parse_with` so parenthesized inner WITH clauses
    /// (e.g. inside a recursive UNION branch) can reuse it.
    fn parse_cte_defs(&mut self) -> Result<(Vec<CteDef>, bool), SqlError> {
        let recursive = self.eat_keyword("recursive");
        let mut ctes = Vec::new();
        loop {
            let name = self.expect_ident()?;
            let col_aliases = if *self.peek() == Token::LParen {
                self.next();
                let mut aliases = Vec::new();
                loop {
                    aliases.push(self.expect_ident()?);
                    if *self.peek() == Token::Comma {
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
            // v0.75: `AS MATERIALIZED` / `AS NOT MATERIALIZED` CTE hints
            // (PG12+). The hint is accepted and ignored — CTEs are always
            // evaluated per reference here.
            if self.eat_keyword("not") {
                self.expect_keyword("materialized")?;
            } else {
                let _ = self.eat_keyword("materialized");
            }
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
            if *self.peek() == Token::Comma {
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
        Ok((ctes, recursive))
    }

    /// v0.10: `WITH [RECURSIVE] name [(cols)] AS (select) [, ...]` followed
    /// by SELECT / INSERT / UPDATE / DELETE.
    fn parse_with(&mut self) -> Result<Stmt, SqlError> {
        let (ctes, _recursive) = self.parse_cte_defs()?;
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
                let mut sel = self.parse_select_query()?;
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
        // v0.77: the body may start with a parenthesized query, e.g.
        // `((VALUES ('a'),('b')) UNION ALL (WITH ... SELECT ...))` as the
        // body of a recursive CTE.
        if *self.peek() == Token::LParen {
            let first = self.parse_select_query_not_consumed()?;
            return self.finish_cte_body(first, recursive);
        }
        // The body must start with SELECT (or VALUES in a non-recursive
        // CTE, v0.59: `WITH v(x) AS (VALUES (1),(2)) SELECT ...`).
        let is_values = match self.next() {
            Token::Ident(kw) if kw == "select" => false,
            Token::Ident(kw) if kw == "values" && !recursive => true,
            other => {
                return Err(err(format!(
                    "syntax error: expected SELECT in CTE body, found {:?}",
                    other
                )));
            }
        };
        if is_values {
            // parse_values_query builds the SelectStmt for a VALUES list.
            return Ok(CteBody::Simple(self.parse_values_query()?));
        }
        // v0.44: a non-recursive CTE body may be a general set operation
        // (`WITH x AS (SELECT ... UNION ...)`, parsed by
        // `parse_select_query`). A recursive CTE keeps the dedicated
        // `non_recursive UNION [ALL] recursive` shape below, so parse its
        // terms without set-operation climbing.
        let first = if recursive {
            self.parse_select_rest()?
        } else {
            self.parse_select_query()?
        };
        self.finish_cte_body(first, recursive)
    }

    /// v0.77: shared tail of `parse_cte_body` — after the first term,
    /// optionally parse `UNION [ALL] <recursive term>`.
    fn finish_cte_body(&mut self, first: SelectStmt, recursive: bool) -> Result<CteBody, SqlError> {
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
            // v0.76: the recursive term may be a parenthesized query
            // (e.g. `(WITH z AS NOT MATERIALIZED (...) SELECT ...)`).
            let second = if *self.peek() == Token::LParen {
                self.parse_select_query_not_consumed()?
            } else {
                match self.next() {
                    Token::Ident(kw) if kw == "select" => {}
                    other => {
                        return Err(err(format!(
                            "syntax error: expected SELECT after UNION in recursive CTE, found {:?}",
                            other
                        )));
                    }
                }
                self.parse_select_rest()?
            };
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
        let columns = if *self.peek() == Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if *self.peek() == Token::Comma {
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
                if *self.peek() == Token::Comma {
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
        // v0.24: general expressions in VALUES. First try the legacy
        // simple-value path (literals, params, typed literals); if the
        // value continues with an operator, rewind and parse a full
        // expression instead.
        let save = self.pos;
        match self.parse_insert_simple() {
            Ok(value) => {
                if matches!(self.peek(), Token::Comma | Token::RParen) {
                    return Ok(value);
                }
            }
            Err(_) => {}
        }
        self.pos = save;
        let expr = self.parse_or()?;
        // Recursively collect parameters (they are substituted before
        // execution, like everywhere else).
        Ok(InsertValue::Expr(expr))
    }

    /// v0.24: the pre-v0.24 simple VALUES parser (literals, params,
    /// typed literals, `::type` suffixes), factored out of
    /// `parse_insert_value`.
    fn parse_insert_simple(&mut self) -> Result<InsertValue, SqlError> {
        // v0.14: typed literals in VALUES, e.g. `bool 't'`, `date '2026-01-01'`
        // (pg_regress conformance). The text is stored as-is; the column
        // coercion applies the target type's input function, matching
        // PostgreSQL assignment semantics (including 22P02 on bad input).
        // v0.36: a double-quoted type name (`"char" 'c'`) works too.
        if let Some(name) = self.peek_ident() {
            let quoted = matches!(self.peek(), Token::QIdent(_));
            // v0.36: quoted type names resolve case-sensitively, so only
            // exact lowercase spellings pass `is_type_start` (`"char"` is
            // the 1-byte type; `"CHAR"` is not a type at all).
            if Self::is_type_start(name) {
                let name = name.to_string();
                let save = self.pos;
                self.next(); // consume the type name
                let ty = if quoted {
                    self.parse_quoted_type_name(name)
                } else {
                    self.parse_type_name_rest(name)
                };
                // v0.81: named composites are not typed literals
                // (composite input syntax is future work); only take this
                // branch for builtin types.
                let is_composite = matches!(&ty, Ok((ColType::Composite, _)));
                if ty.is_ok() && !is_composite {
                    if let Token::Str(s) = self.next() {
                        return Ok(InsertValue::Lit(Literal::Text(s.into())));
                    }
                }
                self.pos = save;
            }
        }
        let value = match self.peek() {
            Token::Param(n) => {
                let n = *n;
                self.next();
                InsertValue::Param(n)
            }
            Token::Minus | Token::Plus => {
                let neg = *self.peek() == Token::Minus;
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
        if *self.peek() == Token::ColonColon {
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
        let left = self.parse_bitor()?;
        // `[NOT] BETWEEN low AND high`, `[NOT] LIKE pat`,
        // `[NOT] ILIKE pat`, `[NOT] IN (subquery)`.
        let neg = self.eat_keyword("not");
        if self.eat_keyword("between") {
            let low = self.parse_bitor()?;
            self.expect_keyword("and")?;
            let high = self.parse_bitor()?;
            return Ok(Expr::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                neg,
            });
        }
        if self.eat_keyword("like") {
            let pattern = self.parse_bitor()?;
            let escape = if self.eat_keyword("escape") {
                Some(Box::new(self.parse_bitor()?))
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
        // v0.31: suppressed while parsing a SUBSTRING subject, where a bare
        // SIMILAR introduces the SQL substring form (handled by the caller).
        if self.allow_similar_to && self.eat_keyword("similar") {
            self.expect_keyword("to")?;
            let pattern = self.parse_bitor()?;
            let mut args = vec![left, pattern];
            if self.eat_keyword("escape") {
                args.push(self.parse_bitor()?);
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
            let pattern = self.parse_bitor()?;
            let escape = if self.eat_keyword("escape") {
                Some(Box::new(self.parse_bitor()?))
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
            // v0.21: `IN (SELECT ...)` is a subquery; `IN (expr, ...)` is a
            // value list, desugared to `left = e1 OR left = e2 ...` (and
            // `NOT (...)` for NOT IN). The OR desugar preserves PG's
            // three-valued logic: NULL comparisons propagate through OR.
            if matches!(self.peek(), Token::Ident(s) if s == "select") {
                let sub = self.parse_subquery()?;
                self.expect(Token::RParen, "')'")?;
                return Ok(Expr::InSub {
                    expr: Box::new(left),
                    sub: Box::new(sub),
                    neg,
                });
            }
            let mut items = if matches!(self.peek(), Token::Ident(s) if s == "values") {
                // v0.54: `IN (VALUES (v1), (v2), ...)` — a VALUES list
                // desugars exactly like a value list. Each row must hold
                // one column (like PG's "subquery has too many columns").
                // v0.87: row-wise `(a, b) IN (VALUES ...)` desugars to an
                // OR of ANDed equalities (the old parse_row_in logic,
                // now reached via `Expr::Row`).
                self.next();
                let rows = self.parse_values_rows()?;
                self.expect(Token::RParen, "')'")?;
                if let Expr::Row(row_items) = &left {
                    return self.desugar_row_in_values(row_items.clone(), rows, neg);
                }
                let mut items = Vec::with_capacity(rows.len());
                for row in rows {
                    if row.len() != 1 {
                        return Err(err(format!(
                            "syntax error: IN (VALUES ...) row has {} columns, expected 1",
                            row.len()
                        )));
                    }
                    items.push(row.into_iter().next().unwrap());
                }
                items
            } else {
                let mut items = vec![self.parse_or()?];
                while *self.peek() == Token::Comma {
                    self.next();
                    items.push(self.parse_or()?);
                }
                self.expect(Token::RParen, "')'")?;
                items
            };
            // v0.87: row-wise `(a, b) IN ((1, 2), (3, 4))` — the list items
            // must be row constructors of matching arity; desugars like
            // the VALUES form.
            if let Expr::Row(row_items) = &left {
                let n = row_items.len();
                let mut rows: Vec<Vec<Expr>> = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Expr::Row(elems) if elems.len() == n => rows.push(elems),
                        _ => {
                            return Err(err(
                                "syntax error: row-wise IN list items must be row constructors of matching arity"
                                    .to_string(),
                            ));
                        }
                    }
                }
                return self.desugar_row_in_values(row_items.clone(), rows, neg);
            }
            let mut expr = Expr::Cmp {
                op: CmpOp::Eq,
                left: Box::new(left.clone()),
                right: Box::new(items.remove(0)),
            };
            for item in items {
                expr = Expr::Or(
                    Box::new(expr),
                    Box::new(Expr::Cmp {
                        op: CmpOp::Eq,
                        left: Box::new(left.clone()),
                        right: Box::new(item),
                    }),
                );
            }
            if neg {
                expr = Expr::Not(Box::new(expr));
            }
            return Ok(expr);
        }
        let op: Option<QuantOp> = match self.peek() {
            Token::Eq => Some(QuantOp::Cmp(CmpOp::Eq)),
            Token::Neq => Some(QuantOp::Cmp(CmpOp::Ne)),
            Token::Lt => Some(QuantOp::Cmp(CmpOp::Lt)),
            Token::LtEq => Some(QuantOp::Cmp(CmpOp::Le)),
            Token::Gt => Some(QuantOp::Cmp(CmpOp::Gt)),
            Token::GtEq => Some(QuantOp::Cmp(CmpOp::Ge)),
            // v0.81: `*=` record-image equality.
            Token::StarEq => Some(QuantOp::Cmp(CmpOp::ImageEq)),
            // v0.87: user-defined operator (e.g. `?=` from CREATE
            // OPERATOR) in expression position (PG19).
            Token::Op(name) => Some(QuantOp::User(name.clone())),
            _ => None,
        };
        let mut expr = match op {
            Some(op) => {
                self.next();
                // v0.76: quantified comparisons `op ANY|ALL|SOME (...)`
                // (PG19). The operand is a subquery or a VALUES list.
                // v0.87: `op` may be a user-defined operator.
                if matches!(self.peek(), Token::Ident(s) if s == "any" || s == "all" || s == "some")
                {
                    let quant = if let Token::Ident(s) = self.next() {
                        s
                    } else {
                        unreachable!()
                    };
                    return self.parse_quantified(left, op, &quant);
                }
                match op {
                    QuantOp::Cmp(cmp) => {
                        let right = self.parse_bitor()?;
                        Expr::Cmp {
                            op: cmp,
                            left: Box::new(left),
                            right: Box::new(right),
                        }
                    }
                    QuantOp::User(name) => {
                        let right = self.parse_bitor()?;
                        Expr::UserOp {
                            op: name,
                            left: Box::new(left),
                            right: Box::new(right),
                        }
                    }
                }
            }
            None => left,
        };
        // `IS [NOT] NULL`, `IS [NOT] TRUE/FALSE/UNKNOWN`,
        // `IS [NOT] DISTINCT FROM`.
        if self.eat_keyword("is") {
            let neg = self.eat_keyword("not");
            // v0.48: `IS [NOT] DISTINCT FROM` (PG19 gram.y) — the
            // NULL-safe comparison. Same precedence level as the other
            // IS forms; the right operand parses at the next-tighter
            // level, like `=` does.
            if self.eat_keyword("distinct") {
                self.expect_keyword("from")?;
                let right = self.parse_bitor()?;
                expr = Expr::IsDistinctFrom {
                    left: Box::new(expr),
                    right: Box::new(right),
                    neg,
                };
            } else {
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
                            "syntax error: expected NULL, TRUE, FALSE, UNKNOWN or DISTINCT after IS, found {:?}",
                            other
                        )));
                    }
                }
            }
        }
        Ok(expr)
    }

    /// v0.76: `expr op ANY|ALL|SOME (subquery | VALUES ...)` — the
    /// quantified comparison (PG19). `ANY`/`SOME` desugar to an OR chain,
    /// `ALL` to an AND chain, preserving PG's three-valued logic. A
    /// `VALUES` list must hold single-column rows. `= ANY (SELECT ...)`
    /// is `IN`, `<> ALL (SELECT ...)` is `NOT IN`.
    /// v0.87: other operators (and user-defined operators like `?=`)
    /// over a subquery produce `Expr::Quantified`, evaluated by the
    /// executor with PG's three-valued logic. `left` may be an
    /// `Expr::Row` for row-wise quantification.
    fn parse_quantified(&mut self, left: Expr, op: QuantOp, quant: &str) -> Result<Expr, SqlError> {
        let quant_kind = if quant == "all" {
            QuantKind::All
        } else {
            QuantKind::Any
        };
        self.expect(Token::LParen, "'('")?;
        // VALUES list form.
        if matches!(self.peek(), Token::Ident(s) if s == "values") {
            self.next();
            let rows = self.parse_values_rows()?;
            self.expect(Token::RParen, "')'")?;
            // v0.87: row-wise `(a, b) = ANY (VALUES ...)` desugars via
            // the row-IN path; other row-wise ops over VALUES are 0A000.
            if let Expr::Row(row_items) = &left {
                match (&op, quant_kind) {
                    (QuantOp::Cmp(CmpOp::Eq), QuantKind::Any) => {
                        return self.desugar_row_in_values(row_items.clone(), rows, false);
                    }
                    (QuantOp::Cmp(CmpOp::Ne), QuantKind::All) => {
                        return self.desugar_row_in_values(row_items.clone(), rows, true);
                    }
                    _ => {
                        return Err(SqlError {
                            message: "row-wise quantified comparison over VALUES is not supported"
                                .to_string(),
                            code: "0A000",
                        });
                    }
                }
            }
            let op = match op {
                QuantOp::Cmp(c) => c,
                QuantOp::User(_) => {
                    return Err(SqlError {
                        message: "user-defined operator over VALUES is not supported".to_string(),
                        code: "0A000",
                    });
                }
            };
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                if row.len() != 1 {
                    return Err(err(format!(
                        "syntax error: quantified comparison VALUES row has {} columns, expected 1",
                        row.len()
                    )));
                }
                items.push(row.into_iter().next().unwrap());
            }
            // Empty set: `= ANY` is false, `<> ALL` is true (PG semantics).
            if items.is_empty() {
                return Ok(Expr::Literal(Literal::Bool(quant == "all")));
            }
            let mut expr = Expr::Cmp {
                op,
                left: Box::new(left.clone()),
                right: Box::new(items.remove(0)),
            };
            for item in items {
                let cmp = Expr::Cmp {
                    op,
                    left: Box::new(left.clone()),
                    right: Box::new(item),
                };
                expr = if quant == "all" {
                    Expr::And(Box::new(expr), Box::new(cmp))
                } else {
                    Expr::Or(Box::new(expr), Box::new(cmp))
                };
            }
            return Ok(expr);
        }
        // Subquery form.
        if matches!(self.peek(), Token::Ident(s) if s == "select") {
            let sub = self.parse_subquery()?;
            self.expect(Token::RParen, "')'")?;
            // `= ANY/SOME` is `IN`, `<> ALL` is `NOT IN` (scalar or row-wise).
            match (&op, quant_kind) {
                (QuantOp::Cmp(CmpOp::Eq), QuantKind::Any) => {
                    return Ok(Expr::InSub {
                        expr: Box::new(left),
                        sub: Box::new(sub),
                        neg: false,
                    });
                }
                (QuantOp::Cmp(CmpOp::Ne), QuantKind::All) => {
                    return Ok(Expr::InSub {
                        expr: Box::new(left),
                        sub: Box::new(sub),
                        neg: true,
                    });
                }
                _ => {}
            }
            return Ok(Expr::Quantified {
                left: Box::new(left),
                op,
                quant: quant_kind,
                sub: Box::new(sub),
            });
        }
        // v0.91: array form — `expr op ANY|ALL|SOME (array_expr)`
        // (PG19). Desugared to the hidden `__any_all_array` builtin
        // (same trick as the `__variadic` marker): the executor
        // iterates the array with PG's three-valued ANY/ALL logic.
        let arr = self.parse_or()?;
        self.expect(Token::RParen, "')'")?;
        let op_txt = match &op {
            QuantOp::Cmp(c) => c.sql().to_string(),
            QuantOp::User(name) => format!("user:{}", name),
        };
        let quant_txt = if quant_kind == QuantKind::All {
            "all"
        } else {
            "any"
        };
        Ok(Expr::Func {
            name: "__any_all_array".to_string(),
            args: vec![
                left,
                Expr::Literal(Literal::Text(op_txt.into())),
                Expr::Literal(Literal::Text(quant_txt.into())),
                arr,
            ],
        })
    }

    /// `SELECT ...` inside parentheses (the `SELECT` keyword not yet consumed).
    fn parse_subquery(&mut self) -> Result<SelectStmt, SqlError> {
        match self.next() {
            Token::Ident(s) if s == "select" => self.parse_select_query(),
            other => Err(err(format!(
                "syntax error: expected SELECT, found {:?}",
                other
            ))),
        }
    }

    /// v0.25: bitwise OR — the loosest bitwise operator (PG binds `|`
    /// looser than `#`, `&`, `<<`/`>>`, and all of those looser than
    /// `||`). Operands are the next-tighter level.
    /// v0.68: the POSIX regex match operators `~`, `!~`, `~*`, `!~*`
    /// share this level — PG19 ranks every "other native operator"
    /// (syntax.sgml) in one left-associative level looser than `+`/`-`
    /// and tighter than LIKE/BETWEEN and the comparisons.
    fn parse_bitor(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_bitxor()?;
        loop {
            let regex = match self.peek() {
                Token::Tilde => Some((false, false)),
                Token::BangTilde => Some((true, false)),
                Token::TildeStar => Some((false, true)),
                Token::BangTildeStar => Some((true, true)),
                _ => None,
            };
            if let Some((not, case_insensitive)) = regex {
                self.next();
                let right = self.parse_bitxor()?;
                left = Expr::Regex {
                    expr: Box::new(left),
                    pattern: Box::new(right),
                    not,
                    case_insensitive,
                };
                continue;
            }
            if *self.peek() != Token::Pipe {
                break;
            }
            self.next();
            let right = self.parse_bitxor()?;
            left = Expr::Arith {
                op: ArithOp::BitOr,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// v0.25: bitwise XOR (`#`), binds tighter than `|` but looser than `&`.
    fn parse_bitxor(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_bitand()?;
        while *self.peek() == Token::Hash {
            self.next();
            let right = self.parse_bitand()?;
            left = Expr::Arith {
                op: ArithOp::BitXor,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// v0.25: bitwise AND (`&`), binds tighter than `#` but looser than shifts.
    fn parse_bitand(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_shift()?;
        while *self.peek() == Token::Amp {
            self.next();
            let right = self.parse_shift()?;
            left = Expr::Arith {
                op: ArithOp::BitAnd,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// v0.25: shifts (`<<`, `>>`), bind looser than `||` (PG ranks `||`
    /// above shifts, so `a || b << 2` is `(a || b) << 2`) but tighter
    /// than `&`.
    fn parse_shift(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_concat()?;
        loop {
            let op = match self.peek() {
                Token::Shl => ArithOp::Shl,
                Token::Shr => ArithOp::Shr,
                _ => break,
            };
            self.next();
            let right = self.parse_concat()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// concat := add (`||` add)*
    fn parse_concat(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_add()?;
        while *self.peek() == Token::PipePipe {
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
        while *self.peek() == Token::Caret {
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
        // v0.79: array subscripts/slices bind tighter than `::`
        // (PG19 indirection sits below the cast in the grammar).
        expr = self.parse_postfix_subscripts(expr)?;
        while *self.peek() == Token::ColonColon {
            self.next();
            let (to, composite_name) = self.parse_type_name()?;
            // v0.81: `::named_composite` becomes CastNamed (resolved at
            // execution time); builtins keep the existing Cast path.
            if let Some(name) = composite_name {
                expr = Expr::CastNamed {
                    expr: Box::new(expr),
                    name,
                };
                expr = self.parse_postfix_subscripts(expr)?;
                continue;
            }
            // Fold `decimal-literal::numeric` to an exact Numeric literal
            // so high-precision decimals don't round-trip through f64.
            // (Postgres parses decimal literals as numeric in the first
            // place; v0.7 keeps float8 for expressions but not for this.)
            // v0.60: only fold for unconstrained `numeric`; a
            // `numeric(p,s)` target must go through the cast so the
            // typmod is applied.
            if let (Expr::Literal(Literal::Decimal(s)), crate::storage::ColType::Numeric(None)) =
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
            // v0.79: subscripts/slices bind to the cast result too —
            // `('...'::int[])[1]` and `('...'::int[])[1]::text`, like
            // PG19 where indirection sits below the cast and above
            // the next `::`.
            expr = self.parse_postfix_subscripts(expr)?;
        }
        Ok(expr)
    }

    /// unary := (`-` | `+`) unary | genprefix | primary, where
    /// genprefix := (`~` | `@` | `|/` | `||/`) cmp.
    /// `-`/`+` sit at PG19's UMINUS precedence (gram.y): tighter than
    /// `^` (so `-2^2` is `(-2)^2`) but looser than `::` (so `-5::int`
    /// is `-(5::int)`). v0.53: `-x` is a first-class `Expr::Neg`
    /// (PG's doNegate) — type-preserving, NULL stays NULL, and
    /// `-'2026-01-01'` fails at evaluation, like Postgres.
    /// The generic prefix operators are PG19's `qual_Op a_expr %prec Op`
    /// — the loosest precedence in the grammar — so their operand is a
    /// full comparison-level expression: `~1 + 1` is `~(1 + 1)`,
    /// `~5::int2` is `~(5::int2)`, `@ 5 - 10` is `@(5 - 10)`. They
    /// desugar like Postgres' parser does: `~x` -> bitwise NOT,
    /// `@x` -> `abs(x)`, `|/x` -> `sqrt(x)`, `||/x` -> `cbrt(x)`.
    fn parse_unary(&mut self) -> Result<Expr, SqlError> {
        match self.peek() {
            Token::Minus => {
                self.next();
                let inner = self.parse_unary()?;
                Ok(Expr::Neg(Box::new(inner)))
            }
            Token::Plus => {
                self.next();
                self.parse_unary()
            }
            Token::Tilde => {
                self.next();
                let inner = self.parse_cmp()?;
                Ok(Expr::BitNot(Box::new(inner)))
            }
            Token::At => {
                self.next();
                let inner = self.parse_cmp()?;
                Ok(Expr::Func {
                    name: "abs".to_string(),
                    args: vec![inner],
                })
            }
            Token::PipeSlash => {
                self.next();
                let inner = self.parse_cmp()?;
                Ok(Expr::Func {
                    name: "sqrt".to_string(),
                    args: vec![inner],
                })
            }
            Token::PipePipeSlash => {
                self.next();
                let inner = self.parse_cmp()?;
                Ok(Expr::Func {
                    name: "cbrt".to_string(),
                    args: vec![inner],
                })
            }
            _ => self.parse_primary(),
        }
    }

    /// v0.55: `CASE [operand] WHEN w THEN r ... [ELSE e] END` (PG19
    /// gram.y: `CASE case_arg when_clause_list case_default END_P`).
    /// The WHEN/THEN/ELSE arms are `a_expr`s. `CASE WHEN ...` (no
    /// operand) is the searched form; otherwise the simple form. Like
    /// PG, an empty WHEN list or a missing END is a syntax error.
    fn parse_case(&mut self) -> Result<Expr, SqlError> {
        // Searched CASE when WHEN follows immediately; otherwise parse
        // the simple-CASE operand. The operand is an a_expr, so it
        // cannot swallow the WHEN keyword.
        let operand = if self.eat_keyword("when") {
            None
        } else {
            let op = self.parse_or()?;
            self.expect_keyword("when")?;
            Some(Box::new(op))
        };
        let mut whens = Vec::new();
        loop {
            // First iteration: WHEN was already consumed above.
            let cond = self.parse_or()?;
            self.expect_keyword("then")?;
            let result = self.parse_or()?;
            whens.push((Box::new(cond), Box::new(result)));
            if !self.eat_keyword("when") {
                break;
            }
        }
        let else_ = if self.eat_keyword("else") {
            Some(Box::new(self.parse_or()?))
        } else {
            None
        };
        self.expect_keyword("end")?;
        Ok(Expr::Case {
            operand,
            whens,
            else_,
        })
    }

    fn parse_primary(&mut self) -> Result<Expr, SqlError> {
        match self.peek() {
            Token::LParen => {
                // v0.75: `((SELECT ...) UNION ...)` — a parenthesized query
                // with set operations (PG19 select_with_parens). Detect a
                // set operator at this paren depth BEFORE consuming `(`.
                let is_setop_paren = self.paren_has_top_level_setop();
                self.next();
                // `(SELECT ...)` = scalar subquery; otherwise parenthesized expr.
                match self.peek() {
                    Token::Ident(s) if s == "select" => {
                        self.next();
                        let sub = self.parse_select_query()?;
                        self.expect(Token::RParen, "')'")?;
                        Ok(Expr::ScalarSub(Box::new(sub)))
                    }
                    _ if is_setop_paren => {
                        // `((SELECT ...) UNION ...)` — the paren was already
                        // consumed; parse the inner query (with its own
                        // parens) plus the set-operation chain.
                        let left = self.parse_select_query_not_consumed()?;
                        let carrier = self.parse_set_chain(left, 1)?;
                        let sub = self.finish_select_query(carrier)?;
                        self.expect(Token::RParen, "')'")?;
                        Ok(Expr::ScalarSub(Box::new(sub)))
                    }
                    _ => {
                        // v0.31: a parenthesized expression is a fresh
                        // context; re-enable SIMILAR TO inside it (it may
                        // have been disabled by an enclosing SUBSTRING).
                        let save_similar = self.allow_similar_to;
                        self.allow_similar_to = true;
                        let first = self.parse_or();
                        self.allow_similar_to = save_similar;
                        let first = first?;
                        if *self.peek() == Token::Comma {
                            // v0.87: row constructor `(e1, e2, ...)` (PG19).
                            // Produces `Expr::Row`; the `[NOT] IN`,
                            // `ANY`/`ALL` handlers deal with it (row-wise
                            // comparisons). Previously this only allowed
                            // row-wise `[NOT] IN (VALUES ...)` and errored
                            // otherwise.
                            let mut items = vec![first];
                            while *self.peek() == Token::Comma {
                                self.next();
                                let save = self.allow_similar_to;
                                self.allow_similar_to = true;
                                let e = self.parse_or();
                                self.allow_similar_to = save;
                                items.push(e?);
                            }
                            self.expect(Token::RParen, "')'")?;
                            return Ok(Expr::Row(items));
                        }
                        self.expect(Token::RParen, "')'")?;
                        Ok(first)
                    }
                }
            }
            Token::Param(n) => {
                let n = *n;
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
                        (Literal::Text(a), Literal::Text(b)) => *a = format!("{}{}", a, b).into(),
                        _ => {
                            return Err(err(
                                "syntax error: adjacent literals must both be strings",
                            ));
                        }
                    }
                }
                Ok(Expr::Literal(lit))
            }
            Token::Ident(s) if s == "true" || s == "false" || s == "null" => {
                Ok(Expr::Literal(self.parse_literal()?))
            }
            Token::Ident(_) => {
                let name = self.expect_ident()?;
                // v0.81: `ROW(...)` row constructor (PG19 `row_expr`).
                // `ROW()`, `ROW(a)`, and `ROW(a, b, ...)` all produce
                // `Expr::Row`; field names are PG's `f1`, `f2`, ...
                if name == "row" && *self.peek() == Token::LParen {
                    self.next();
                    let mut elems = Vec::new();
                    if *self.peek() != Token::RParen {
                        loop {
                            elems.push(self.parse_or()?);
                            if *self.peek() == Token::Comma {
                                self.next();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(Token::RParen, "')'")?;
                    return Ok(Expr::Row(elems));
                }
                // v0.79: `ARRAY[...]` constructor (PG19 `array_expr`).
                // `ARRAY[1,2]`, `ARRAY[]`, and the nested form
                // `ARRAY[[1,2],[3,4]]`. (`array(SELECT...)` stays in
                // parse_call.)
                if name == "array" && *self.peek() == Token::LBracket {
                    return self.parse_array_ctor();
                }
                // `EXISTS (SELECT ...)` — only when followed by `(` so a
                // column actually named "exists" still works elsewhere.
                if name == "exists" && *self.peek() == Token::LParen {
                    self.next();
                    let sub = self.parse_subquery()?;
                    self.expect(Token::RParen, "')'")?;
                    return Ok(Expr::Exists {
                        sub: Box::new(sub),
                        neg: false,
                    });
                }
                // v0.55: `CASE [operand] WHEN ... THEN ... [ELSE ...]
                // END`. CASE is reserved in PG19, so the keyword always
                // starts a CASE expression (a column literally named
                // "case" must be double-quoted).
                if name == "case" {
                    return self.parse_case();
                }
                // `CAST(x AS type)` — special form, not a function call.
                if name == "cast" && *self.peek() == Token::LParen {
                    self.next();
                    let expr = self.parse_or()?;
                    self.expect_keyword("as")?;
                    let (to, composite_name) = self.parse_type_name()?;
                    self.expect(Token::RParen, "')'")?;
                    // v0.81: CAST(x AS named_composite).
                    if let Some(name) = composite_name {
                        return Ok(Expr::CastNamed {
                            expr: Box::new(expr),
                            name,
                        });
                    }
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
                    // v0.81: named composites are not typed literals.
                    if let Ok((to, None)) = self.parse_type_name_rest(name.clone()) {
                        if let Token::Str(s) = self.peek() {
                            let s = s.clone();
                            self.next();
                            return Ok(Expr::Cast {
                                expr: Box::new(Expr::Literal(Literal::Text(s.into()))),
                                to,
                            });
                        }
                    }
                    self.pos = save;
                }
                // Aggregate / built-in function call `name(...)`?
                if *self.peek() == Token::LParen {
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
                if *self.peek() == Token::Dot {
                    self.next();
                    // v0.73: `qual.*` in expression position is PG19's
                    // whole-row Var (varattno = 0), not a column ref.
                    if *self.peek() == Token::Star {
                        self.next();
                        return Ok(Expr::WholeRow { qual: name });
                    }
                    let col = self.expect_ident()?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    });
                }
                Ok(Expr::Column { table: None, name })
            }
            // v0.36: double-quoted identifier in expression position. Like
            // PG, quoted names are never keywords: no EXISTS/CAST/typed-
            // literal special forms (except `"char" 'c'`), but quoted
            // function names, qualified refs, and (function-style) casts
            // work.
            Token::QIdent(name) => {
                let name = name.clone();
                self.next();
                // Typed literal `"char" 'c'` (and `"date" '...'`, etc.):
                // backtrack when no string literal follows, like unquoted.
                if Self::is_type_start(&name) {
                    let save = self.pos;
                    // v0.81: named composites are not typed literals.
                    if let Ok((to, None)) = self.parse_quoted_type_name(name.clone()) {
                        if let Token::Str(s) = self.peek() {
                            let s = s.clone();
                            self.next();
                            return Ok(Expr::Cast {
                                expr: Box::new(Expr::Literal(Literal::Text(s.into()))),
                                to,
                            });
                        }
                    }
                    self.pos = save;
                }
                // Function-style cast `"char"(expr)`: parse_call would
                // resolve the name to character(1), so handle it here.
                if name == "char" && *self.peek() == Token::LParen {
                    self.next();
                    let expr = self.parse_or()?;
                    self.expect(Token::RParen, "')'")?;
                    return Ok(Expr::Cast {
                        expr: Box::new(expr),
                        to: ColType::SingleChar,
                    });
                }
                // Quoted function call `"name"(...)`?
                if *self.peek() == Token::LParen {
                    return self.parse_call(name);
                }
                // Qualified ref `"table".column`?
                if *self.peek() == Token::Dot {
                    self.next();
                    // v0.73: `qual.*` in expression position is PG19's
                    // whole-row Var (varattno = 0), not a column ref.
                    if *self.peek() == Token::Star {
                        self.next();
                        return Ok(Expr::WholeRow { qual: name });
                    }
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

    /// v0.54: the tail of a row constructor `(e1, ..., en)` parsed by
    /// `parse_primary`: expect `[NOT] IN (VALUES ...)` and desugar to
    /// `(e1=r11 AND ... AND en=r1n) OR (e1=r21 AND ...) ...`
    /// (`NOT (...)` for `NOT IN`). The AND/OR desugar preserves PG's
    /// three-valued row-comparison logic. Any other use of a row
    /// constructor — including row-wise `IN (subquery)` — is a syntax
    /// error here.
    /// v0.87: row-wise `(a, b, ...) [NOT] IN (VALUES ...)` desugared to
    /// an OR of ANDed equalities, preserving PG's three-valued logic
    /// (NULL comparisons propagate through AND/OR). Refactored from the
    /// v0.54 `parse_row_in` (which consumed the `IN (VALUES ...)` itself);
    /// the row now arrives as `Expr::Row` and the VALUES rows are parsed
    /// by the caller.
    fn desugar_row_in_values(
        &mut self,
        items: Vec<Expr>,
        rows: Vec<Vec<Expr>>,
        neg: bool,
    ) -> Result<Expr, SqlError> {
        let n = items.len();
        let mut expr: Option<Expr> = None;
        for row in rows {
            if row.len() != n {
                return Err(err(format!(
                    "syntax error: IN (VALUES ...) row has {} columns, expected {}",
                    row.len(),
                    n
                )));
            }
            let mut conj: Option<Expr> = None;
            for (item, val) in items.iter().zip(row.iter()) {
                let term = Expr::Cmp {
                    op: CmpOp::Eq,
                    left: Box::new(item.clone()),
                    right: Box::new(val.clone()),
                };
                conj = Some(match conj {
                    None => term,
                    Some(c) => Expr::And(Box::new(c), Box::new(term)),
                });
            }
            // parse_values_rows guarantees at least one row.
            let disjunct = conj.unwrap();
            expr = Some(match expr {
                None => disjunct,
                Some(e) => Expr::Or(Box::new(e), Box::new(disjunct)),
            });
        }
        // parse_values_rows guarantees at least one row.
        let mut expr = expr.unwrap();
        if neg {
            expr = Expr::Not(Box::new(expr));
        }
        Ok(expr)
    }

    /// `count(*)`, `count(e)`, `sum(e)`, `avg(e)`, `min(e)`, `max(e)`.
    /// Anything else followed by `(` is "function does not exist" (42883
    /// at execution type-check; here a plain syntax-level error naming it).
    /// `name(...)` — aggregates, EXTRACT/TRIM/POSITION/SUBSTRING special
    /// forms, and the v0.7 built-in function set. Anything else is
    /// "function does not exist" (SQLSTATE 42883).
    /// v0.79: `ARRAY[...]` — PG19's `array_expr`. Handles `ARRAY[]`
    /// (empty), `ARRAY[1,2]` (element list), and the nested
    /// `ARRAY[[1,2],[3,4]]` form. The `[` has already been peeked
    /// (not consumed).
    fn parse_array_ctor(&mut self) -> Result<Expr, SqlError> {
        self.next(); // consume '['
        if *self.peek() == Token::RBracket {
            self.next();
            return Ok(Expr::ArrayCtor {
                elems: Vec::new(),
                nested: false,
            });
        }
        // Nested form: each element is itself a bracketed array.
        if *self.peek() == Token::LBracket {
            let mut rows = vec![self.parse_bracketed_array()?];
            while *self.peek() == Token::Comma {
                self.next();
                rows.push(self.parse_bracketed_array()?);
            }
            self.expect(Token::RBracket, "']'")?;
            return Ok(Expr::ArrayCtor {
                elems: rows,
                nested: true,
            });
        }
        let mut elems = vec![self.parse_or()?];
        while *self.peek() == Token::Comma {
            self.next();
            elems.push(self.parse_or()?);
        }
        self.expect(Token::RBracket, "']'")?;
        Ok(Expr::ArrayCtor {
            elems,
            nested: false,
        })
    }

    /// v0.79: one `[...]` level of the nested `ARRAY[[...],[...]]`
    /// form, parsed recursively (deeper nesting stacks dims).
    fn parse_bracketed_array(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LBracket, "'['")?;
        if *self.peek() == Token::RBracket {
            self.next();
            return Ok(Expr::ArrayCtor {
                elems: Vec::new(),
                nested: false,
            });
        }
        if *self.peek() == Token::LBracket {
            let mut rows = vec![self.parse_bracketed_array()?];
            while *self.peek() == Token::Comma {
                self.next();
                rows.push(self.parse_bracketed_array()?);
            }
            self.expect(Token::RBracket, "']'")?;
            return Ok(Expr::ArrayCtor {
                elems: rows,
                nested: true,
            });
        }
        let mut elems = vec![self.parse_or()?];
        while *self.peek() == Token::Comma {
            self.next();
            elems.push(self.parse_or()?);
        }
        self.expect(Token::RBracket, "']'")?;
        Ok(Expr::ArrayCtor {
            elems,
            nested: false,
        })
    }

    /// v0.79: postfix array subscripts and slices. Adjacent brackets
    /// form ONE subscript/slice operation (PG19 gram.y /
    /// transformIndirection): `a[i][j]` is a single two-index
    /// subscript; if any bracket is `l:u` the whole chain is a slice
    /// and a plain `[i]` becomes `[1:i]` (PG19
    /// array_subscript_transform). More than 6 brackets is PG19's
    /// 54000 "number of array dimensions exceeds the maximum allowed
    /// (6)". Binds tighter than `::`, like PG19's indirection.
    fn parse_postfix_subscripts(&mut self, mut expr: Expr) -> Result<Expr, SqlError> {
        // v0.81: composite field access `(expr).field` (PG19 indirection).
        // Qualified `tbl.col` is consumed in the Ident branch, so a `.`
        // here always follows a parenthesized/composite expression.
        // Multiple levels `(r).a.b` nest left-associatively.
        loop {
            match self.peek() {
                Token::Dot => {
                    // `tbl.*` whole-row is handled in the Ident branch;
                    // a `.` here must be followed by a field name.
                    self.next(); // consume '.'
                    let field = self.expect_ident()?;
                    expr = Expr::FieldAccess {
                        expr: Box::new(expr),
                        field,
                    };
                }
                _ => break,
            }
        }
        // (lower, upper, is_slice) per bracket pair.
        let mut items: Vec<(Option<Expr>, Option<Expr>, bool)> = Vec::new();
        while *self.peek() == Token::LBracket {
            self.next(); // consume '['
            if *self.peek() == Token::Colon {
                // `[:upper]` — null lower bound.
                self.next();
                let upper = if *self.peek() == Token::RBracket {
                    None
                } else {
                    Some(self.parse_or()?)
                };
                self.expect(Token::RBracket, "']'")?;
                items.push((None, upper, true));
            } else {
                let first = self.parse_or()?;
                if *self.peek() == Token::Colon {
                    self.next();
                    let upper = if *self.peek() == Token::RBracket {
                        None
                    } else {
                        Some(self.parse_or()?)
                    };
                    self.expect(Token::RBracket, "']'")?;
                    items.push((Some(first), upper, true));
                } else {
                    self.expect(Token::RBracket, "']'")?;
                    items.push((None, Some(first), false));
                }
            }
        }
        if items.is_empty() {
            return Ok(expr);
        }
        if items.len() > 6 {
            return Err(SqlError {
                message: format!(
                    "number of array dimensions ({}) exceeds the maximum allowed (6)",
                    items.len()
                ),
                code: "54000",
            });
        }
        if items.iter().any(|(_, _, s)| *s) {
            let one = Expr::Literal(Literal::Int(1));
            let bounds = items
                .into_iter()
                .map(|(l, u, s)| {
                    if s {
                        (l.map(Box::new), u.map(Box::new))
                    } else {
                        (Some(Box::new(one.clone())), u.map(Box::new))
                    }
                })
                .collect();
            expr = Expr::Slice {
                array: Box::new(expr),
                bounds,
            };
        } else {
            let indices = items
                .into_iter()
                .map(|(_, u, _)| u.expect("non-slice bracket always has an index"))
                .collect();
            expr = Expr::Subscript {
                array: Box::new(expr),
                indices,
            };
        }
        Ok(expr)
    }

    fn parse_call(&mut self, name: String) -> Result<Expr, SqlError> {
        // v0.77: `array(SELECT ...)` — ARRAY constructor with a subquery.
        // This is valid PG syntax; without it the 42601 would (correctly)
        // abort an explicit transaction in the conformance suite.
        if name == "array"
            && *self.peek() == Token::LParen
            && matches!(self.peek2(), Token::Ident(s) if s == "select")
        {
            self.next(); // consume '('
            self.next(); // consume 'select'
            let sub = self.parse_select_query()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(Expr::ArraySubquery(Box::new(sub)));
        }
        // v0.14: function-style cast — PostgreSQL treats `typename(expr)`
        // as a cast when the name is a type and there is exactly one
        // argument (e.g. `float8(count(*))`).
        if Self::is_type_start(&name) {
            let save = self.pos;
            // v0.81: named composites are not func-style casts.
            if let Ok((to, None)) = self.parse_type_name_rest(name.clone()) {
                if *self.peek() == Token::LParen {
                    self.next();
                    if let Ok(expr) = self.parse_or() {
                        if *self.peek() == Token::RParen {
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
            // v0.80: `GROUPING(...)` is PG19's grouping-set mask function
            // (a dedicated grammar production, not a regular function).
            // Quoted variants (e.g. `"Grouping"(...)`) keep the generic
            // path — only the folded keyword form is special.
            "grouping" => return self.parse_grouping(),
            _ => {}
        }
        let agg = match name.as_str() {
            "count" => Some(AggFunc::Count),
            "sum" => Some(AggFunc::Sum),
            "avg" => Some(AggFunc::Avg),
            "min" => Some(AggFunc::Min),
            "max" => Some(AggFunc::Max),
            "string_agg" => Some(AggFunc::StringAgg),
            "bool_and" => Some(AggFunc::BoolAnd),
            "variance" | "var_samp" => Some(AggFunc::VarianceSamp),
            "var_pop" => Some(AggFunc::VariancePop),
            "stddev" | "stddev_samp" => Some(AggFunc::StddevSamp),
            "stddev_pop" => Some(AggFunc::StddevPop),
            "array_agg" => Some(AggFunc::ArrayAgg),
            _ => None,
        };
        if let Some(func) = agg {
            self.expect(Token::LParen, "'('")?;
            let distinct = self.eat_keyword("distinct");
            if distinct && matches!(func, AggFunc::Count) && *self.peek() == Token::Star {
                return Err(err("syntax error: DISTINCT is not allowed with count(*)"));
            }
            let arg = if matches!(func, AggFunc::Count) && *self.peek() == Token::Star {
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
            // v0.92: `agg(x ORDER BY ...)` — PG19 allows ORDER BY inside
            // any aggregate call (docs §4.2.7). With DISTINCT, PG
            // requires every ORDER BY expression to match an aggregate
            // argument (parse_agg.c).
            let mut args = Vec::new();
            if let Some(a) = arg {
                args.push(a);
            }
            if let Some(a) = arg2 {
                args.push(a);
            }
            let agg_order_by = if self.eat_keyword("order") {
                self.expect_keyword("by")?;
                let mut terms = Vec::new();
                loop {
                    terms.push(self.parse_order_term()?);
                    if *self.peek() == Token::Comma {
                        self.next();
                        continue;
                    }
                    break;
                }
                if distinct && !terms.iter().all(|t| args.iter().any(|a| **a == t.expr)) {
                    // v0.92: PG19 parse_clause.c transformDistinctClause
                    // (is_agg=true): 42P10 with this exact message.
                    return Err(SqlError {
                        message: "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list".to_string(),
                        code: "42P10",
                    });
                }
                terms
            } else {
                Vec::new()
            };
            self.expect(Token::RParen, "')'")?;
            let mut expr = Expr::Agg {
                func: func.clone(),
                arg: args.first().cloned(),
                distinct,
                arg2: args.get(1).cloned(),
                agg_order_by: agg_order_by.clone(),
            };
            // v0.10: `<agg>(...) OVER (...)` — windowed aggregate.
            if self.eat_keyword("over") {
                // v0.92: PG19 parse_func.c rejects ORDER BY inside a
                // windowed aggregate call: 0A000 "aggregate ORDER BY is
                // not implemented for window functions".
                if !agg_order_by.is_empty() {
                    return Err(SqlError {
                        message: "aggregate ORDER BY is not implemented for window functions"
                            .to_string(),
                        code: "0A000",
                    });
                }
                let spec = self.parse_window_spec()?;
                expr = Expr::Window {
                    func: WindowFunc::Agg(func),
                    args: args.into_iter().map(|a| *a).collect(),
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
        if *self.peek() == Token::LParen {
            self.next();
            let mut args = Vec::new();
            if *self.peek() != Token::RParen {
                loop {
                    // v0.90: `VARIADIC expr` — marks the argument for
                    // array expansion (PG19). Parsed as a `__variadic`
                    // marker func; exec.rs expands it in function-call
                    // evaluation.
                    if self.eat_keyword("variadic") {
                        let inner = self.parse_or()?;
                        args.push(Expr::Func {
                            name: "__variadic".to_string(),
                            args: vec![inner],
                        });
                    } else {
                        args.push(self.parse_or()?);
                    }
                    if *self.peek() == Token::Comma {
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
                if *self.peek() == Token::Comma {
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
                order_by.push(self.parse_order_term()?);
                if *self.peek() == Token::Comma {
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
        // v0.33: PG resolves SQL trim syntax to btrim/ltrim/rtrim
        // (the column name is the function name).
        self.expect(Token::LParen, "'('")?;
        let mut spec = "both".to_string();
        if matches!(self.peek(), Token::Ident(s) if s == "leading" || s == "trailing" || s == "both")
        {
            if let Token::Ident(s) = self.next() {
                spec = s;
            }
        }
        let func_name = match spec.as_str() {
            "leading" => "ltrim",
            "trailing" => "rtrim",
            _ => "btrim",
        }
        .to_string();
        if self.eat_keyword("from") {
            let s = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            // trim([spec] from str) -> btrim/ltrim/rtrim(str)
            return Ok(Expr::Func {
                name: func_name,
                args: vec![s],
            });
        }
        let first = self.parse_or()?;
        if self.eat_keyword("from") {
            let s = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            // trim([spec] chars from str) -> btrim/ltrim/rtrim(str, chars)
            return Ok(Expr::Func {
                name: func_name,
                args: vec![s, first],
            });
        }
        self.expect(Token::RParen, "')'")?;
        // trim(str) -> btrim(str)
        Ok(Expr::Func {
            name: "btrim".to_string(),
            args: vec![first],
        })
    }

    /// `position(sub in str)` or `position(sub, str)`.
    fn parse_position(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        // Parse below `IN` (parse_cmp) so a trailing `in` is left for the
        // `position(x in y)` form instead of becoming an IN-subquery.
        let a = self.parse_bitor()?;
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

    /// v0.80: `GROUPING(a, b, ...)` — PG19's grouping-set mask function.
    /// The grammar production takes a non-empty expression list, so zero
    /// arguments are a syntax error (42601), like Postgres. Represented
    /// as `Func { name: "grouping" }`; argument validation (fewer than 32
    /// arguments, each matching a grouping expression of the query level)
    /// and mask evaluation happen at group level in exec.
    fn parse_grouping(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        if *self.peek() == Token::RParen {
            return Err(err("syntax error: GROUPING requires at least one argument"));
        }
        let mut args = Vec::new();
        loop {
            args.push(self.parse_or()?);
            if *self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        self.expect(Token::RParen, "')'")?;
        Ok(Expr::Func {
            name: "grouping".to_string(),
            args,
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
            let ln = if *self.peek() == Token::Comma {
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
        // v0.31: parse the subject with SIMILAR TO disabled, so that a bare
        // SIMILAR after the subject introduces the SQL substring form
        // (`SUBSTRING(s SIMILAR pat [ESCAPE c])`) instead of erroring on a
        // missing TO. Parenthesized sub-expressions re-enable it (see
        // parse_primary).
        let save_similar = self.allow_similar_to;
        self.allow_similar_to = false;
        let s = self.parse_or();
        self.allow_similar_to = save_similar;
        let s = s?;
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
                // v0.24: `SUBSTRING(s FROM x FOR y)` — integer
                // (start, len) vs SIMILAR (pattern, escape) is decided
                // at runtime by type (PG), so `-1`, `1+1`, etc. work.
                return Ok(Expr::Func {
                    name: "substring_from_for".to_string(),
                    args: vec![s, pat_or_start, for_arg],
                });
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
        if *self.peek() == Token::Comma {
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
            Token::Ident(s) if !is_reserved(s) => {
                let s = s.clone();
                self.next();
                Ok(Some(s))
            }
            // v0.36: a double-quoted alias may be any word, even a
            // reserved one (like PG).
            Token::QIdent(s) => {
                let s = s.clone();
                self.next();
                Ok(Some(s))
            }
            // v0.24: bare U&"..." alias; expect_ident handles UESCAPE.
            Token::UIdent(_) => Ok(Some(self.expect_ident()?)),
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
            if *self.peek() == Token::Comma {
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
        // SET [ SESSION | LOCAL ] name = value | SET name TO value |
        // SET name TO DEFAULT. v0.66: `SET LOCAL` is distinguished from
        // `SET`/`SET SESSION`: it is transaction-scoped (the value
        // reverts when the transaction ends, whether committed or not,
        // like PG19). `SET LOCAL` outside a transaction block is a
        // 25001 error, raised at execution time.
        let local = self.eat_keyword("local");
        if !local {
            self.eat_keyword("session");
        }
        let name = self.expect_ident()?;
        if self.eat_keyword("to") {
            // consumed TO
        } else if *self.peek() == Token::Eq {
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
        Ok(Stmt::Set { name, value, local })
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
    /// v0.22: UPDATE/DELETE WHERE is a full predicate expression
    /// (previously only `col = literal` was accepted). Mirrors the
    /// ON CONFLICT DO UPDATE ... WHERE parsing.
    fn parse_where_expr_opt(&mut self) -> Result<Option<Expr>, SqlError> {
        if self.eat_keyword("where") {
            Ok(Some(self.parse_or()?))
        } else {
            Ok(None)
        }
    }

    fn parse_update(&mut self) -> Result<Stmt, SqlError> {
        let table = self.expect_ident()?;
        // v0.76: `UPDATE t AS x` / `UPDATE t x` (PG's alias clause).
        let alias = self.parse_alias_opt()?;
        self.expect_keyword("set")?;
        let mut sets = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(Token::Eq, "'='")?;
            let expr = self.parse_or()?;
            sets.push((col, expr));
            if *self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        if sets.is_empty() {
            return Err(err("syntax error: UPDATE requires at least one assignment"));
        }
        // v0.76: `UPDATE ... FROM <from-items>` (PG19) — extra tables the
        // SET/WHERE/RETURNING clauses may reference, like SELECT's FROM.
        let from = if self.eat_keyword("from") {
            self.parse_from()?
        } else {
            Vec::new()
        };
        let where_ = self.parse_where_expr_opt()?;
        let returning = self.parse_returning()?;
        Ok(Stmt::Update {
            table,
            alias,
            sets,
            from,
            where_,
            returning,
            with: Vec::new(),
        })
    }

    fn parse_delete(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("from")?;
        let table = self.expect_ident()?;
        // v0.65: `DELETE FROM t AS dt` / `DELETE FROM t dt` (PG's alias
        // clause; the alias becomes the visible qualifier).
        let alias = self.parse_alias_opt()?;
        // v0.74: `DELETE FROM t USING <from-items>` (PG19) — extra
        // tables the WHERE clause may reference, like SELECT's FROM.
        let using = if self.eat_keyword("using") {
            self.parse_from()?
        } else {
            Vec::new()
        };
        let where_ = self.parse_where_expr_opt()?;
        let returning = self.parse_returning()?;
        Ok(Stmt::Delete {
            table,
            alias,
            using,
            where_,
            returning,
            with: Vec::new(),
        })
    }

    // --- v0.74: PREPARE / EXECUTE / DEALLOCATE (PG19) ------------------
    // `PREPARE name [(type, ...)] AS statement`.
    fn parse_prepare(&mut self) -> Result<Stmt, SqlError> {
        let name = self.expect_ident()?;
        let types = if *self.peek() == Token::LParen {
            self.next();
            let mut ts = Vec::new();
            loop {
                // v0.81: named composites in PREPARE type lists map to
                // "unknown" (no param typing for composites yet).
                let (ct, _) = self.parse_type_name()?;
                // Normalized name for the arity check / error messages.
                let tn = match ct {
                    crate::storage::ColType::Int => "integer",
                    crate::storage::ColType::BigInt => "bigint",
                    crate::storage::ColType::SmallInt => "smallint",
                    crate::storage::ColType::Float => "double precision",
                    crate::storage::ColType::Float4 => "real",
                    crate::storage::ColType::Numeric(_) => "numeric",
                    crate::storage::ColType::Text => "text",
                    crate::storage::ColType::Char(_) => "character",
                    crate::storage::ColType::Varchar(_) => "character varying",
                    crate::storage::ColType::SingleChar => "\"char\"",
                    crate::storage::ColType::Bool => "boolean",
                    crate::storage::ColType::Date => "date",
                    crate::storage::ColType::Timestamp => "timestamp",
                    crate::storage::ColType::Timestamptz => "timestamptz",
                    crate::storage::ColType::Bytea => "bytea",
                    crate::storage::ColType::Uuid => "uuid",
                    _ => "unknown",
                };
                ts.push(tn.to_string());
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
            ts
        } else {
            Vec::new()
        };
        self.expect_keyword("as")?;
        let inner = self.parse_top()?;
        Ok(Stmt::Prepare {
            name,
            types,
            stmt: Box::new(inner),
        })
    }

    // `EXECUTE name [(expr, ...)]`.
    fn parse_execute(&mut self) -> Result<Stmt, SqlError> {
        let name = self.expect_ident()?;
        let args = if *self.peek() == Token::LParen {
            self.next();
            let mut es = Vec::new();
            if *self.peek() == Token::RParen {
                self.next();
            } else {
                loop {
                    es.push(self.parse_or()?);
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
            }
            es
        } else {
            Vec::new()
        };
        Ok(Stmt::Execute { name, args })
    }

    // `DEALLOCATE name` / `DEALLOCATE ALL`.
    fn parse_deallocate(&mut self) -> Result<Stmt, SqlError> {
        let name = if self.eat_keyword("all") {
            None
        } else {
            Some(self.expect_ident()?)
        };
        Ok(Stmt::Deallocate { name })
    }

    // --- v0.16: TRUNCATE / SQL cursors -----------------------------------
    fn parse_truncate(&mut self) -> Result<Stmt, SqlError> {
        // TRUNCATE [ TABLE ] name [, ...]
        //   [ RESTART IDENTITY | CONTINUE IDENTITY ] [ CASCADE | RESTRICT ]
        let _ = self.eat_keyword("table");
        let mut tables = vec![self.expect_ident()?];
        while *self.peek() == Token::Comma {
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
        let query = self.parse_select_query()?;
        Ok(Stmt::Declare {
            name,
            query,
            with_hold,
        })
    }

    /// An optional `-` followed by an integer literal, for FETCH counts.
    fn parse_fetch_count(&mut self) -> Result<i64, SqlError> {
        let neg = *self.peek() == Token::Minus && {
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
        if *self.peek() == Token::Minus {
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
        // v0.89: FETCH [ direction [ FROM | IN ] ] name — PG19 lets the
        // FROM/IN keywords be omitted (`FETCH ok` == `FETCH NEXT FROM ok`).
        let dir = self.parse_fetch_dir()?;
        let _ = self.eat_keyword("from") || self.eat_keyword("in");
        let name = self.expect_ident()?;
        Ok(Stmt::Fetch { name, dir })
    }

    fn parse_move(&mut self) -> Result<Stmt, SqlError> {
        // MOVE [ direction [ FROM | IN ] ] name — FROM/IN optional (PG19).
        let dir = self.parse_fetch_dir()?;
        let _ = self.eat_keyword("from") || self.eat_keyword("in");
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
        // v0.75: `VACUUM (options) table` — parenthesized option list
        // (PG12+). Options are parsed and the relevant ones (analyze,
        // verbose) are honored; others are accepted and ignored.
        let mut verbose = self.eat_keyword("verbose");
        let mut analyze = self.eat_keyword("analyze");
        if *self.peek() == Token::LParen {
            self.next();
            loop {
                if self.eat_keyword("analyze") {
                    analyze = true;
                } else if self.eat_keyword("verbose") {
                    verbose = true;
                } else {
                    // Skip unknown option: `name [value]`.
                    let _ = self.expect_ident();
                    // Optional value (boolean, number, or string).
                    if matches!(self.peek(), Token::Ident(_))
                        && !matches!(self.peek(), Token::Ident(s) if s == "analyze" || s == "verbose")
                    {
                        // Could be a value; peek ahead for comma or ')'.
                        // Simpler: if next is comma or ')', it was a flag.
                        // We already consumed the ident; check what's next.
                    }
                }
                match self.next() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    Token::Ident(_) => {
                        // `option value` — value was an ident; expect comma or ')'.
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
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')', found {:?}",
                            other
                        )));
                    }
                }
            }
        }
        let table = match self.peek() {
            Token::Ident(_) | Token::QIdent(_) => Some(self.expect_ident()?),
            _ => None,
        };
        Ok(Stmt::Vacuum {
            table,
            verbose,
            analyze,
        })
    }

    /// The body of a SELECT after the `SELECT` keyword was consumed.
    /// v0.44: the SELECT core: `SELECT [DISTINCT] ... HAVING`, without the
    /// trailing `ORDER BY` / `LIMIT` / `OFFSET` / `FOR UPDATE` (see
    /// `parse_select_tail`) and without set-operation climbing (see
    /// `parse_select_query`). Used for set-operation branches, where
    /// Postgres forbids a tail before the operator outside parentheses.
    fn parse_select_core(&mut self) -> Result<SelectStmt, SqlError> {
        // v0.52: `SELECT DISTINCT ON (expr [, ...])` (PG19 gram.y
        // DistinctClause). Like Postgres, ON after DISTINCT always
        // introduces DISTINCT ON — `SELECT DISTINCT on FROM t` is a
        // syntax error, not DISTINCT over a column named `on`.
        let mut distinct = false;
        let mut distinct_on: Vec<Expr> = Vec::new();
        if self.eat_keyword("distinct") {
            if self.eat_keyword("on") {
                self.expect(Token::LParen, "'('")?;
                loop {
                    distinct_on.push(self.parse_or()?);
                    if *self.peek() == Token::Comma {
                        self.next();
                        continue;
                    }
                    break;
                }
                self.expect(Token::RParen, "')'")?;
            } else {
                distinct = true;
            }
        } else {
            // ALL is the default; consume it if present.
            self.eat_keyword("all");
        };
        let mut items = Vec::new();
        loop {
            // v0.87: zero-target-list SELECT (`SELECT WHERE false`): if no
            // items yet and WHERE follows, stop (PG19 allows it). Other
            // clauses (FROM/GROUP/etc.) still require a target list.
            if items.is_empty() {
                if let Token::Ident(kw) = self.peek() {
                    if kw.as_str() == "where" {
                        break;
                    }
                }
            }
            // `*`
            if *self.peek() == Token::Star {
                self.next();
                items.push(SelectItem::All);
            } else if let Some(q) = self.peek_ident() {
                // v0.36: `qual.*` with a double-quoted qualifier.
                let q = q.to_string();
                // `qual.*` — but only when a `*` really follows the dot;
                // `qual.col` is a normal expression.
                if *self.peek2() == Token::Dot && *self.peek3() == Token::Star {
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
            if *self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        // v0.87: PG19 allows a zero-target-list SELECT (`SELECT WHERE
        // false`); the empty list is only valid if WHERE follows.
        if items.is_empty() {
            let ok = matches!(self.peek(), Token::Ident(s) if s.as_str() == "where");
            if !ok {
                return Err(err("syntax error: SELECT requires a select list"));
            }
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
        let (group_by, group_by_sets) = if self.eat_keyword("group") {
            self.expect_keyword("by")?;
            self.parse_group_by()?
        } else {
            (Vec::new(), false)
        };
        let having = if self.eat_keyword("having") {
            Some(self.parse_or()?)
        } else {
            None
        };
        Ok(SelectStmt {
            with: Vec::new(),
            distinct,
            distinct_on,
            items,
            from,
            where_,
            group_by,
            group_by_sets,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            for_update: false,
            set_op: None,
        })
    }

    /// v0.44: trailing `ORDER BY` / `LIMIT` / `OFFSET` / `FOR UPDATE` of a
    /// SELECT. Split out of `parse_select_rest` so set-operation branches
    /// can be parsed without consuming the outer query's tail (Postgres
    /// forbids `ORDER BY` before a set-op outside parentheses).
    /// v0.54: one `ORDER BY` sort term: `expr [ASC | DESC | USING op]
    /// [NULLS FIRST | NULLS LAST]`. PG19's gram.y `sortby` is
    /// `a_expr USING qual_all_Op | a_expr ASC | a_expr DESC | a_expr`,
    /// so `USING` is exclusive with ASC/DESC (the sort operator alone
    /// determines the direction), but `NULLS FIRST | NULLS LAST` may
    /// still follow `USING`. Only the btree `<`/`>` sort operators map
    /// to a direction; anything else is 0A000 (not implemented), not a
    /// syntax error.
    fn parse_order_term(&mut self) -> Result<OrderTerm, SqlError> {
        let expr = self.parse_or()?;
        let desc = if self.eat_keyword("using") {
            match self.next() {
                Token::Lt => false,
                Token::Gt => true,
                other => {
                    return Err(SqlError {
                        message: format!("ORDER BY USING with operator {other:?} is not supported"),
                        code: "0A000",
                    });
                }
            }
        } else if self.eat_keyword("desc") {
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
        Ok(OrderTerm {
            expr,
            desc,
            nulls_first,
        })
    }

    fn parse_select_tail(&mut self, sel: &mut SelectStmt) -> Result<(), SqlError> {
        let order_by = if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            let mut terms = Vec::new();
            loop {
                terms.push(self.parse_order_term()?);
                if *self.peek() == Token::Comma {
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
                            return Err(err(format!("syntax error: bad LIMIT value \"{raw}\"")));
                        }
                    },
                    other => {
                        return Err(err(format!(
                            "syntax error: expected LIMIT count, found {other:?}"
                        )));
                    }
                }
            } else if offset.is_none() && self.eat_keyword("offset") {
                match self.next() {
                    Token::Number(raw) => match raw.parse::<i64>() {
                        Ok(n) => offset = Some(n),
                        Err(_) => {
                            return Err(err(format!("syntax error: bad OFFSET value \"{raw}\"")));
                        }
                    },
                    other => {
                        return Err(err(format!(
                            "syntax error: expected OFFSET count, found {other:?}"
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
        sel.order_by = order_by;
        sel.limit = limit;
        sel.offset = offset;
        sel.for_update = for_update;
        Ok(())
    }

    /// v0.78: `GROUP BY` with PG's grouping-element syntax: `()`,
    /// `ROLLUP (...)`, `CUBE (...)`, `GROUPING SETS (...)` (nestable),
    /// and `DISTINCT`. Returns the grouping sets — the cross product of
    /// the top-level elements — plus whether grouping-set syntax was
    /// used (which switches unbound bare columns from 42803 to NULL,
    /// like PG).
    fn parse_group_by(&mut self) -> Result<(Vec<Vec<Expr>>, bool), SqlError> {
        // PG allows `GROUP BY DISTINCT ...` (dedups identical sets).
        let distinct = self.eat_keyword("distinct");
        let mut sets: Vec<Vec<Expr>> = vec![Vec::new()];
        let mut is_sets = distinct;
        loop {
            let (elem_sets, elem_is_sets) = self.parse_grouping_element()?;
            is_sets = is_sets || elem_is_sets;
            let mut next = Vec::with_capacity(sets.len() * elem_sets.len());
            for prefix in &sets {
                for elem in &elem_sets {
                    let mut combined = prefix.clone();
                    combined.extend(elem.iter().cloned());
                    next.push(combined);
                }
            }
            sets = next;
            if *self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        if distinct {
            sets = Self::dedup_grouping_sets(sets);
        }
        Ok((sets, is_sets))
    }

    /// v0.78: one grouping element → its grouping sets plus whether it
    /// used grouping-set syntax. `()` is the empty set; `ROLLUP`/`CUBE`
    /// expand; `GROUPING SETS` flattens (nesting included); anything else
    /// is one plain expression.
    fn parse_grouping_element(&mut self) -> Result<(Vec<Vec<Expr>>, bool), SqlError> {
        // v0.78: only treat ROLLUP/CUBE/GROUPING as grouping-set syntax
        // when followed by `(` (resp. the SETS keyword); otherwise they
        // are ordinary column names.
        let is_rollup = matches!(self.peek(), Token::Ident(s) if s == "rollup")
            && *self.peek2() == Token::LParen;
        let is_cube =
            matches!(self.peek(), Token::Ident(s) if s == "cube") && *self.peek2() == Token::LParen;
        let is_grouping_sets = matches!(self.peek(), Token::Ident(s) if s == "grouping")
            && matches!(self.peek2(), Token::Ident(s) if s == "sets");
        if is_rollup {
            self.next(); // rollup
            self.expect(Token::LParen, "'('")?;
            let mut exprs = Vec::new();
            loop {
                exprs.push(self.parse_or()?);
                if *self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, ")")?;
            // ROLLUP (a, b) -> (a,b), (a), ().
            let mut sets = Vec::with_capacity(exprs.len() + 1);
            for k in (0..=exprs.len()).rev() {
                sets.push(exprs[..k].to_vec());
            }
            return Ok((sets, true));
        }
        if is_cube {
            self.next(); // cube
            self.expect(Token::LParen, "(")?;
            let mut exprs = Vec::new();
            loop {
                exprs.push(self.parse_or()?);
                if *self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, ")")?;
            // CUBE (a, b) -> (a,b), (a), (b), ().
            // v0.78: cap at 16 elements (65536 sets); beyond that the
            // exponential expansion is a footgun, so fail loudly rather
            // than silently dropping columns.
            if exprs.len() > 16 {
                return Err(err(
                    "syntax error: CUBE with more than 16 elements is not supported",
                ));
            }
            let n = exprs.len();
            let mut sets = Vec::with_capacity(1 << n);
            for mask in (0..(1u32 << n)).rev() {
                let mut set = Vec::new();
                for (i, e) in exprs.iter().enumerate().take(n) {
                    if mask & (1 << (n - 1 - i)) != 0 {
                        set.push(e.clone());
                    }
                }
                sets.push(set);
            }
            return Ok((sets, true));
        }
        if is_grouping_sets {
            self.next(); // grouping
            self.expect_keyword("sets")?;
            self.expect(Token::LParen, "(")?;
            let mut sets = Vec::new();
            loop {
                let (elem_sets, _) = self.parse_grouping_element()?;
                sets.extend(elem_sets);
                if *self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, ")")?;
            return Ok((sets, true));
        }
        if *self.peek() == Token::LParen && *self.peek2() == Token::RParen {
            // The empty `()` grouping set.
            self.next(); // '('
            self.next(); // ')'
            return Ok((vec![Vec::new()], true));
        }
        Ok((vec![vec![self.parse_or()?]], false))
    }

    /// v0.78: deduplicate identical grouping sets, preserving first-seen
    /// order (PG's `GROUP BY DISTINCT ...`). Set equality is structural
    /// on the expressions.
    fn dedup_grouping_sets(sets: Vec<Vec<Expr>>) -> Vec<Vec<Expr>> {
        let mut out: Vec<Vec<Expr>> = Vec::with_capacity(sets.len());
        for s in sets {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    /// The pre-v0.44 `parse_select_rest`: a simple SELECT with no
    /// set-operation climbing. Kept for recursive-CTE terms (which detect
    /// their own `UNION`) and as the branch parser for set operations.
    fn parse_select_rest(&mut self) -> Result<SelectStmt, SqlError> {
        let mut sel = self.parse_select_core()?;
        self.parse_select_tail(&mut sel)?;
        Ok(sel)
    }

    /// v0.44: parse a full query: a simple SELECT optionally followed by a
    /// `UNION` / `INTERSECT` / `EXCEPT` chain and trailing `ORDER BY` /
    /// `LIMIT` / `OFFSET`. Precedence follows Postgres: `INTERSECT` binds
    /// tighter than `UNION`/`EXCEPT`; equal-precedence ops are
    /// left-associative. A query with set-ops is returned as a carrier
    /// `SelectStmt` whose `set_op` holds the branches; a plain SELECT is
    /// returned exactly as `parse_select_rest` would produce it.
    fn parse_select_query(&mut self) -> Result<SelectStmt, SqlError> {
        let left = self.parse_set_branch()?;
        let carrier = self.parse_set_chain(left, 1)?;
        self.finish_select_query(carrier)
    }

    /// v0.44: one set-operation branch where the SELECT keyword was
    /// already consumed (the leftmost branch of `parse_select_query`).
    /// v0.49: per PG19 gram.y only a target_list can follow SELECT, so
    /// a `(` here always opens a parenthesized *expression* (or scalar
    /// subquery), never a parenthesized set branch — it must go through
    /// parse_select_core, whose parse_primary handles `(SELECT ...)`.
    /// The old `(` arm misparsed `SELECT (expr)` (e.g. `SELECT (-1)`)
    /// as a query ("expected SELECT, found Minus") and silently
    /// unwrapped scalar subqueries (`SELECT (SELECT id FROM users)`
    /// lost its 21000 multi-row check).
    fn parse_set_branch(&mut self) -> Result<SelectStmt, SqlError> {
        self.parse_select_core()
    }

    /// v0.44: parse a full query when the SELECT keyword has NOT been
    /// consumed yet (right-hand branches of set operators, and
    /// parenthesized queries). Delegates to `parse_select_query` once
    /// SELECT is consumed.
    fn parse_select_query_not_consumed(&mut self) -> Result<SelectStmt, SqlError> {
        if *self.peek() == Token::LParen {
            self.next();
            let inner = self.parse_select_query_not_consumed()?;
            self.expect(Token::RParen, "')'")?;
            Ok(inner)
        } else if matches!(self.peek(), Token::Ident(s) if s == "values") {
            // v0.54: `(VALUES ...)` — a parenthesized VALUES branch.
            self.next();
            self.parse_values_query()
        } else if matches!(self.peek(), Token::Ident(s) if s == "table") {
            // v0.54: `(TABLE name)` — a parenthesized TABLE branch.
            self.next();
            self.parse_table_query()
        } else if matches!(self.peek(), Token::Ident(s) if s == "with") {
            // v0.76: `(WITH ... SELECT ...)` — a parenthesized query with
            // its own CTEs (e.g. inside a recursive UNION branch).
            self.next();
            let (ctes, _recursive) = self.parse_cte_defs()?;
            self.expect_keyword("select")?;
            let mut sel = self.parse_select_query()?;
            sel.with = ctes;
            Ok(sel)
        } else {
            self.expect_keyword("select")?;
            self.parse_select_query()
        }
    }

    /// v0.44: parse one operand of a set operator (the right-hand side).
    /// This is a branch core with NO trailing `ORDER BY` / `LIMIT` /
    /// `OFFSET` — those belong to the outermost root and are parsed by
    /// `finish_select_query`. A parenthesized operand is a full query
    /// (its own tail stays inside the parens).
    fn parse_set_operand(&mut self) -> Result<SelectStmt, SqlError> {
        if *self.peek() == Token::LParen {
            self.next();
            let inner = self.parse_select_query_not_consumed()?;
            self.expect(Token::RParen, "')'")?;
            Ok(inner)
        } else if matches!(self.peek(), Token::Ident(s) if s == "values") {
            // v0.54: PG19 `simple_select: values_clause` — a VALUES list is
            // a full set-operation branch, like SELECT.
            self.next();
            Ok(Self::values_branch(self.parse_values_rows()?))
        } else if matches!(self.peek(), Token::Ident(s) if s == "table") {
            // v0.54: PG19 `simple_select: TABLE relation_expr`.
            self.next();
            Ok(Self::table_branch(self.parse_table_name()?))
        } else {
            self.expect_keyword("select")?;
            self.parse_select_core()
        }
    }

    /// v0.54: desugar a standalone `VALUES` row list into the SELECT the
    /// executor already understands: `SELECT * FROM (VALUES ...)`. Used
    /// both for top-level `VALUES` queries and for set-op branches.
    fn values_branch(rows: Vec<Vec<Expr>>) -> SelectStmt {
        let mut sel = empty_select();
        sel.items = vec![SelectItem::All];
        sel.from = vec![FromItem::Values {
            rows,
            // PG auto-names the VALUES RTE; the name never surfaces.
            alias: "_values".to_string(),
            col_aliases: Vec::new(),
        }];
        sel
    }

    /// v0.54: desugar `TABLE name` into `SELECT * FROM name`.
    fn table_branch(name: String) -> SelectStmt {
        let mut sel = empty_select();
        sel.items = vec![SelectItem::All];
        sel.from = vec![FromItem::Table {
            name,
            alias: None,
            col_aliases: Vec::new(),
        }];
        sel
    }

    /// v0.54: `relation_expr` for `TABLE name` — an optionally
    /// schema-qualified table name, like `FROM name`.
    fn parse_table_name(&mut self) -> Result<String, SqlError> {
        let name = self.expect_ident()?;
        if *self.peek() == Token::Dot {
            self.next();
            Ok(format!("{}.{}", name, self.expect_ident()?))
        } else {
            Ok(name)
        }
    }

    /// v0.54: the `(expr, ...) [, ...]` row list of a `VALUES` clause
    /// (the `VALUES` keyword itself already consumed). Shared by the
    /// FROM-item parser, top-level `VALUES` queries, and set-op branches.
    fn parse_values_rows(&mut self) -> Result<Vec<Vec<Expr>>, SqlError> {
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
            if *self.peek() == Token::Comma {
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
        Ok(rows)
    }

    /// v0.54: PG19 `simple_select: values_clause` as a top-level query:
    /// `VALUES (...) [, ...] [set-ops] [ORDER BY ...] [LIMIT ...]`.
    fn parse_values_query(&mut self) -> Result<SelectStmt, SqlError> {
        let rows = self.parse_values_rows()?;
        let left = Self::values_branch(rows);
        let carrier = self.parse_set_chain(left, 1)?;
        self.finish_select_query(carrier)
    }

    /// v0.54: PG19 `simple_select: TABLE relation_expr` as a top-level
    /// query: `TABLE name [set-ops] [ORDER BY ...] [LIMIT ...]`.
    fn parse_table_query(&mut self) -> Result<SelectStmt, SqlError> {
        let name = self.parse_table_name()?;
        let left = Self::table_branch(name);
        let carrier = self.parse_set_chain(left, 1)?;
        self.finish_select_query(carrier)
    }

    /// v0.44: shared tail of `parse_select_query`: parse the trailing
    /// `ORDER BY` / `LIMIT` / `OFFSET` / `FOR UPDATE` and attach it to the
    /// carrier (or to the plain SELECT when there are no set-ops).
    fn finish_select_query(&mut self, mut carrier: SelectStmt) -> Result<SelectStmt, SqlError> {
        let mut tail = empty_select();
        self.parse_select_tail(&mut tail)?;
        if let Some(root) = carrier.set_op.as_mut() {
            if tail.for_update {
                return Err(err(
                    "syntax error: FOR UPDATE is not supported with set operations".to_string(),
                ));
            }
            root.order_by = tail.order_by;
            root.limit = tail.limit;
            root.offset = tail.offset;
        } else {
            // No set-ops: plain SELECT; the tail belongs to it directly.
            carrier.order_by = tail.order_by;
            carrier.limit = tail.limit;
            carrier.offset = tail.offset;
            carrier.for_update = tail.for_update;
        }
        Ok(carrier)
    }

    /// v0.44: precedence-climbing for set operations. Parses
    /// `left (OP [ALL|DISTINCT] right)*` for ops with precedence >=
    /// `min_prec` (`INTERSECT` = 2, `UNION`/`EXCEPT` = 1). Returns `left`
    /// with the chain accumulated on its carrier `set_op` (created on
    /// first use), so equal-precedence ops stay left-associative while
    /// tighter ops nest inside the right branch.
    fn parse_set_chain(
        &mut self,
        mut left: SelectStmt,
        min_prec: u8,
    ) -> Result<SelectStmt, SqlError> {
        // v0.44: the first operator in this invocation always creates a new
        // carrier around `left` — even if `left` is already a carrier from
        // a parenthesized subquery (which keeps its own ORDER BY/LIMIT).
        // Subsequent operators in the same invocation append to that new
        // carrier, preserving left-associativity for equal-precedence ops
        // while tighter ops nest inside the right branch.
        let mut first = true;
        loop {
            let (op, prec) = match self.peek() {
                Token::Ident(kw) if kw == "union" && 1 >= min_prec => (SetOpKind::Union, 1),
                Token::Ident(kw) if kw == "intersect" && 2 >= min_prec => (SetOpKind::Intersect, 2),
                Token::Ident(kw) if kw == "except" && 1 >= min_prec => (SetOpKind::Except, 1),
                _ => break,
            };
            self.next(); // consume the operator keyword
            // `ALL` keeps duplicates; bare or `DISTINCT` deduplicates.
            let all = if self.eat_keyword("all") {
                true
            } else {
                self.eat_keyword("distinct");
                false
            };
            let branch_stmt = self.parse_set_operand()?;
            let right = self.parse_set_chain(branch_stmt, prec + 1)?;
            let branch = SetOpBranch {
                op,
                all,
                right: Box::new(right),
            };
            if first {
                // First set-op in this invocation: wrap `left` as the
                // carrier's left child.
                let branch_stmt = std::mem::replace(&mut left, empty_select());
                left.set_op = Some(Box::new(SetOpRoot {
                    left: Box::new(branch_stmt),
                    chain: vec![branch],
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                }));
                first = false;
            } else if let Some(root) = left.set_op.as_mut() {
                // Subsequent equal-precedence op: append to our carrier.
                root.chain.push(branch);
            }
        }
        Ok(left)
    }

    /// FROM source [, ...] — commas become CROSS JOINs; explicit JOINs
    /// bind tighter than commas.
    fn parse_from(&mut self) -> Result<Vec<FromItem>, SqlError> {
        let mut items = vec![self.parse_join_chain()?];
        while *self.peek() == Token::Comma {
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
                using_alias: None,
                alias: None,
                col_aliases: Vec::new(),
            };
        }
        Ok(vec![acc])
    }

    /// v0.74: true when the next tokens start another join in a FROM chain
    /// (`[NATURAL] [INNER|LEFT|RIGHT|FULL|CROSS] JOIN`). Used for PostgreSQL's
    /// shift-preferred join grammar: when a join's right operand is followed
    /// immediately by another join (no ON/USING yet), the right operand
    /// extends rightward (`a LEFT JOIN b LEFT JOIN c ON p1 ON p2` =
    /// `a LEFT JOIN (b LEFT JOIN c ON p1) ON p2`).
    fn peek_join_start(&self) -> bool {
        match self.peek() {
            Token::Ident(s)
                if s == "join"
                    || s == "inner"
                    || s == "left"
                    || s == "right"
                    || s == "full"
                    || s == "cross"
                    || s == "natural" =>
            {
                true
            }
            _ => false,
        }
    }

    fn parse_join_chain(&mut self) -> Result<FromItem, SqlError> {
        let left = self.parse_from_primary()?;
        self.parse_join_rest(left)
    }

    /// v0.74: right-recursive join parsing matching PostgreSQL's grammar.
    /// `parse_join_chain` parses the first operand, then this consumes the
    /// join tail: after each join's right operand, if another join keyword
    /// follows (no ON/USING seen yet), that join nests into the right
    /// operand (bison shift preference); once ON/USING attaches, parsing
    /// continues left-deep as before.
    fn parse_join_rest(&mut self, mut left: FromItem) -> Result<FromItem, SqlError> {
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
            let mut right = self.parse_from_primary()?;
            // v0.74: PG shift preference — a join keyword immediately after
            // the right operand (no ON/USING yet) extends the right operand.
            if self.peek_join_start() {
                right = self.parse_join_rest(right)?;
            }
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
                            if *self.peek() == Token::Comma {
                                self.next();
                                continue;
                            }
                            break;
                        }
                        self.expect(Token::RParen, "')'")?;
                        (None, cols)
                    } else {
                        // v0.74: PG19 requires ON/USING for INNER/LEFT/
                        // RIGHT/FULL JOIN (only CROSS and NATURAL omit
                        // it); a missing qualifier is a syntax error.
                        self.expect_keyword("on")?;
                        (Some(self.parse_or()?), Vec::new())
                    }
                }
            };
            // v0.23: `JOIN ... USING (cols) AS alias` — the alias belongs to
            // the USING clause (exposes only the merged columns); the `AS`
            // keyword is required, like PostgreSQL.
            let using_alias = if !using.is_empty() && self.eat_keyword("as") {
                Some(self.expect_ident()?)
            } else {
                None
            };
            left = FromItem::Join {
                left: Box::new(left),
                kind,
                right: Box::new(right),
                on,
                using,
                natural,
                using_alias,
                alias: None,
                col_aliases: Vec::new(),
            };
        }
        Ok(left)
    }

    fn parse_from_primary(&mut self) -> Result<FromItem, SqlError> {
        // v0.87: explicit `LATERAL` (PG19). Only table functions are
        // supported under it; LATERAL derived tables / plain tables get
        // an honest 0A000 below.
        let lateral = self.eat_keyword("lateral");
        if *self.peek() == Token::LParen {
            self.next();
            // v0.14: PostgreSQL allows redundant parens: FROM ((SELECT ...)).
            // Only consume an extra '(' when it opens a subquery or VALUES —
            // never a VALUES row tuple like (1, 2).
            let mut extra = 0;
            while *self.peek() == Token::LParen
                && matches!(self.peek2(), Token::Ident(s) if s == "select" || s == "values")
            {
                self.next();
                extra += 1;
            }
            let mut item = if matches!(self.peek(), Token::Ident(s) if s == "values") {
                self.next();
                let rows = self.parse_values_rows()?;
                FromItem::Values {
                    rows,
                    alias: String::new(),
                    col_aliases: Vec::new(),
                }
            } else if matches!(self.peek(), Token::Ident(s) if s == "select" || s == "with") {
                // v0.76: `(WITH ... SELECT ...)` derived tables are now
                // supported (PG19). Parse via parse_select_query_not_consumed
                // which handles the leading WITH.
                let sub = self.parse_select_query_not_consumed()?;
                FromItem::Derived {
                    sub: Box::new(sub),
                    alias: String::new(),
                    col_aliases: Vec::new(),
                }
            } else {
                // v0.23: parenthesized joined table (or bare table):
                // `(a JOIN b ...)`, `(tbl)`.
                self.parse_join_chain()?
            };
            // v0.75: `((query) [AS] alias [JOIN ...])` — a parenthesized
            // derived table that is the left operand of a join, e.g.
            // `((select ...) s LEFT JOIN t ...)`. If we consumed extra '('
            // and the derived table's ')' is not followed by another ')',
            // the extra '(' was the derived table's own paren, not a
            // redundant one.
            // Tracks whether the inner derived table already got its alias
            // (so the outer alias becomes optional).
            let mut inner_aliased = false;
            if extra > 0 && matches!(item, FromItem::Derived { .. } | FromItem::Values { .. }) {
                // Consume the derived table's ')'.
                self.expect(Token::RParen, "')'")?;
                extra -= 1;
                if *self.peek() != Token::RParen {
                    // An alias follows: `((query) alias ...)`.
                    let inner_alias = self.parse_alias_opt()?.unwrap_or_default();
                    let inner_cols = self.parse_col_alias_list()?;
                    match &mut item {
                        FromItem::Values {
                            alias: a,
                            col_aliases: c,
                            ..
                        } => {
                            *a = inner_alias;
                            *c = inner_cols;
                        }
                        FromItem::Derived {
                            alias: a,
                            col_aliases: c,
                            ..
                        } => {
                            *a = inner_alias;
                            *c = inner_cols;
                        }
                        _ => {}
                    }
                    inner_aliased = true;
                    // A join may follow the aliased derived table.
                    item = self.parse_join_rest(item)?;
                }
                // Else: redundant parens `((query))`; the outer alias is
                // parsed below.
            }
            for _ in 0..=extra {
                self.expect(Token::RParen, "')'")?;
            }
            // The alias follows the closing parens: FROM ((SELECT 1 AS x)) ss.
            // v0.21: `AS t(x, y)` column aliases for VALUES/derived tables.
            // v0.23: `(a JOIN b ...) [AS] x [(cols)]` — the alias is
            // optional here (unlike derived tables) and never auto-generated:
            // without one the inner table names stay visible.
            let (alias, col_aliases) = match &item {
                FromItem::Join { .. } | FromItem::Table { .. } => {
                    let a = self.parse_alias_opt()?;
                    let c = self.parse_col_alias_list()?;
                    // v0.23: PostgreSQL requires the table alias when a
                    // column alias list is present.
                    if a.is_none() && !c.is_empty() {
                        return Err(err(
                            "syntax error: column aliases require a table alias".to_string()
                        ));
                    }
                    (a, c)
                }
                // v0.75: if the inner derived table already has its alias
                // (`((query) alias ...)`), the outer alias is optional.
                _ if inner_aliased => {
                    let a = self.parse_alias_opt()?;
                    let c = self.parse_col_alias_list()?;
                    (a, c)
                }
                _ => {
                    let a = self.parse_derived_alias()?;
                    let c = self.parse_col_alias_list()?;
                    (Some(a), c)
                }
            };
            match &mut item {
                FromItem::Values {
                    alias: a,
                    col_aliases: c,
                    ..
                } => {
                    // v0.75: don't clobber an inner alias when the outer
                    // alias is absent (`((query) alias)`).
                    if alias.is_some() || !inner_aliased {
                        *a = alias.unwrap_or_default();
                    }
                    if !col_aliases.is_empty() || !inner_aliased {
                        *c = col_aliases;
                    }
                }
                FromItem::Derived {
                    alias: a,
                    col_aliases: c,
                    ..
                } => {
                    // v0.75: don't clobber an inner alias when the outer
                    // alias is absent (`((query) alias)`).
                    if alias.is_some() || !inner_aliased {
                        *a = alias.unwrap_or_default();
                    }
                    if !col_aliases.is_empty() || !inner_aliased {
                        *c = col_aliases;
                    }
                }
                FromItem::Join {
                    alias: a,
                    col_aliases: c,
                    ..
                } => {
                    *a = alias;
                    *c = col_aliases;
                }
                FromItem::Table {
                    alias: a,
                    col_aliases: c,
                    ..
                } => {
                    if a.is_some() && alias.is_some() {
                        return Err(err(format!(
                            "table name \"{}\" specified more than once",
                            a.as_deref().unwrap_or("")
                        )));
                    }
                    if alias.is_some() {
                        *a = alias;
                    }
                    *c = col_aliases;
                }
                // v0.32: unreachable here (functions parse in the
                // non-parenthesized branch with alias already set).
                FromItem::Function { .. } => {}
            }
            // v0.87: explicit LATERAL only supports table functions;
            // `LATERAL (subquery)` / `LATERAL (VALUES ...)` is 0A000.
            if lateral {
                return Err(SqlError {
                    message: "LATERAL is only supported on table functions".to_string(),
                    code: "0A000",
                });
            }
            Ok(item)
        } else {
            let name = self.expect_ident()?;
            // v0.9: schema-qualified names, so the information_schema
            // catalog views are reachable (`FROM information_schema.tables`).
            let name = if *self.peek() == Token::Dot {
                self.next();
                format!("{}.{}", name, self.expect_ident()?)
            } else {
                name
            };
            // v0.32: `FROM func(args)` — set-returning table function.
            if *self.peek() == Token::LParen {
                self.next();
                let mut args = Vec::new();
                if *self.peek() != Token::RParen {
                    loop {
                        args.push(self.parse_or()?);
                        if *self.peek() == Token::Comma {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(Token::RParen, "')'")?;
                let alias = self.parse_alias_opt()?;
                let col_aliases = self.parse_col_alias_list()?;
                return Ok(FromItem::Function {
                    name,
                    args,
                    alias,
                    col_aliases,
                    lateral,
                });
            }
            let alias = self.parse_alias_opt()?;
            // v0.20: `FROM tbl [AS] x (a, b, c)` — optional column aliases.
            let col_aliases = self.parse_col_alias_list()?;
            // v0.87: explicit LATERAL is only supported on table
            // functions; `LATERAL tbl` / `LATERAL (subquery)` is 0A000.
            if lateral {
                return Err(SqlError {
                    message: "LATERAL is only supported on table functions".to_string(),
                    code: "0A000",
                });
            }
            Ok(FromItem::Table {
                name,
                alias,
                col_aliases,
            })
        }
    }

    /// `(a, b, c)` column alias list after a table alias, or empty when
    /// the next token is not `(`.
    fn parse_col_alias_list(&mut self) -> Result<Vec<String>, SqlError> {
        if *self.peek() == Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if *self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, "')'")?;
            Ok(cols)
        } else {
            Ok(Vec::new())
        }
    }

    /// Alias for a parenthesized FROM item. PostgreSQL auto-generates
    /// `unnamed_subquery[_N]` when no alias is given (v0.14).
    fn parse_derived_alias(&mut self) -> Result<String, SqlError> {
        if self.eat_keyword("as") {
            return self.expect_ident();
        }
        match self.peek() {
            Token::Ident(s) if !is_reserved(s) => {
                let s = s.clone();
                self.next();
                Ok(s)
            }
            // v0.36: a double-quoted derived-table alias may be any word
            // (like PG).
            Token::QIdent(s) => {
                let s = s.clone();
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
        if matches!(self.peek(), Token::Ident(s) if s == "role" || s == "user" || s == "group") {
            return self.parse_drop_role();
        }
        if self.eat_keyword("index") {
            let if_exists = if self.eat_keyword("if") {
                self.expect_keyword("exists")?;
                true
            } else {
                false
            };
            // v0.88: `DROP INDEX a, b, c` (PG allows a name list).
            let mut names = Vec::new();
            loop {
                names.push(self.expect_ident()?);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.next(); // ','
            }
            return Ok(Stmt::DropIndex { names, if_exists });
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
        // v0.22: DROP TYPE [IF EXISTS] name [, ...] [CASCADE | RESTRICT].
        // CASCADE/RESTRICT are accepted; dependents are not tracked
        // (bounded shell-type support).
        if self.eat_keyword("type") {
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
            self.parse_cascade_opt()?;
            return Ok(Stmt::DropType { names, if_exists });
        }
        // v0.85: DROP DOMAIN [IF EXISTS] name [, ...] [CASCADE | RESTRICT]
        // — mirrors DROP TYPE (dependents are not tracked).
        if self.eat_keyword("domain") {
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
            self.parse_cascade_opt()?;
            return Ok(Stmt::DropDomain { names, if_exists });
        }
        // v0.86: DROP FUNCTION [IF EXISTS] name ([type [, ...]]).
        // Argument names are accepted and ignored (PG allows them).
        if matches!(self.peek(), Token::Ident(s) if s == "function") {
            return self.parse_drop_function();
        }
        // v0.86: DROP OPERATOR [IF EXISTS] name (lefttype, righttype)
        // (PG19; use NONE for a missing side).
        if matches!(self.peek(), Token::Ident(s) if s == "operator") {
            return self.parse_drop_operator();
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
                        // v0.36: double-quoted column names.
                        Token::Ident(n) | Token::QIdent(n) => columns.push(n),
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
        // v0.76: the other datetime functions are 0-argument builtins
        // in PG19 (now(3) etc. are 42883); arity enforced in
        // check_builtin_arity.
        | "clock_timestamp" | "statement_timestamp" | "transaction_timestamp"
        // conditional
        | "coalesce" | "nullif" | "greatest" | "least"
        // v0.9: sequence functions
        | "nextval" | "currval" | "setval"
        // v0.14: PostgreSQL internal operator-function aliases
        | "booleq" | "boolne" | "int4eq" | "texteq"
        // v0.73: row_to_json(record) -> json (json.c).
        | "row_to_json"
        // v0.79: array functions (PG19 arrayfuncs.c).
        | "array_length" | "cardinality" | "array_dims" | "array_ndims"
        | "array_lower" | "array_upper" | "unnest"
    )
}

/// Arity check for built-in functions. Wrong argument counts raise
/// "function does not exist" (SQLSTATE 42883), like Postgres.
pub fn check_builtin_arity(name: &str, n: usize) -> Result<(), SqlError> {
    let ok = match name {
        "upper" | "lower" | "length" | "char_length" | "character_length" | "abs" | "floor"
        | "ceil" | "ceiling" | "sqrt" => n == 1,
        "current_date" => n == 0,
        // v0.76: only CURRENT_TIMESTAMP takes the optional precision
        // (0-6) — PG19's grammar handles it as special syntax
        // (makeSQLValueFunction), while now()/clock_timestamp()/
        // statement_timestamp()/transaction_timestamp() are plain
        // 0-argument pg_proc functions (now(3) is 42883 in PG19).
        "current_timestamp" => n <= 1,
        "now" | "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => n == 0,
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
        // v0.73: row_to_json(record) takes exactly one argument.
        "row_to_json" => n == 1,
        // v0.79: array functions (PG19 arrayfuncs.c arities).
        "array_length" => n == 2,
        "cardinality" => n == 1,
        "array_dims" => n == 1,
        "array_ndims" => n == 1,
        "array_lower" | "array_upper" => n == 2,
        "unnest" => n == 1,
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
            // v0.32: a table function is not a relation for dependency
            // tracking.
            FromItem::Function { .. } => {}
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
        allow_similar_to: true,
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
        Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::UserOp { .. }
        | Expr::Exists { .. } => Err(err(format!("cannot use subquery in {} constraint", what))),
        Expr::Param(_) => Err(err(format!("cannot use parameter in {} constraint", what))),
        Expr::ResolvedCol { .. } => Err(err(format!("invalid expression in {}", what))),
        Expr::Column { .. } | Expr::WholeRow { .. } | Expr::Literal(_) => Ok(()),
        Expr::Arith { left, right, .. } => {
            validate_constraint_expr(left, what)?;
            validate_constraint_expr(right, what)
        }
        // v0.81: composite expressions — validate sub-expressions.
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => {
            validate_constraint_expr(expr, what)
        }
        Expr::Row(elems) => {
            for e in elems {
                validate_constraint_expr(e, what)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => validate_constraint_expr(expr, what),
        // v0.79: array constructors/subscripts are fine in CHECK /
        // DEFAULT (no subqueries, no aggregates inside them — enforced
        // by recursing).
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                validate_constraint_expr(e, what)?;
            }
            Ok(())
        }
        Expr::Subscript { array, indices } => {
            validate_constraint_expr(array, what)?;
            for i in indices {
                validate_constraint_expr(i, what)?;
            }
            Ok(())
        }
        Expr::Slice { array, bounds } => {
            validate_constraint_expr(array, what)?;
            for (l, u) in bounds {
                if let Some(l) = l {
                    validate_constraint_expr(l, what)?;
                }
                if let Some(u) = u {
                    validate_constraint_expr(u, what)?;
                }
            }
            Ok(())
        }
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            validate_constraint_expr(a, what)?;
            validate_constraint_expr(b, what)
        }
        Expr::Not(a) => validate_constraint_expr(a, what),
        Expr::BitNot(a) => validate_constraint_expr(a, what),
        Expr::Neg(a) => validate_constraint_expr(a, what),
        // v0.55: CASE is allowed in CHECK/DEFAULT — validate arms.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                validate_constraint_expr(o, what)?;
            }
            for (k, r) in whens {
                validate_constraint_expr(k, what)?;
                validate_constraint_expr(r, what)?;
            }
            if let Some(e) = else_ {
                validate_constraint_expr(e, what)?;
            }
            Ok(())
        }
        Expr::Like { expr, pattern, .. } => {
            validate_constraint_expr(expr, what)?;
            validate_constraint_expr(pattern, what)
        }
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
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
        Expr::IsDistinctFrom { left, right, .. } => {
            validate_constraint_expr(left, what)?;
            validate_constraint_expr(right, what)
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
        // v0.73: whole-row Var.
        Expr::WholeRow { qual } => {
            out.push_str("(wholerow ");
            sexpr_escape(qual, out);
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
                ArithOp::BitAnd => "bitand",
                ArithOp::BitOr => "bitor",
                ArithOp::BitXor => "bitxor",
                ArithOp::Shl => "shl",
                ArithOp::Shr => "shr",
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
        // v0.81: composite expressions.
        Expr::CastNamed { expr, name } => {
            out.push_str("(castnamed ");
            sexpr_escape(name, out);
            out.push(' ');
            encode_expr_inner(expr, out);
            out.push(')');
        }
        Expr::Row(elems) => {
            out.push_str("(row");
            for e in elems {
                out.push(' ');
                encode_expr_inner(e, out);
            }
            out.push(')');
        }
        Expr::FieldAccess { expr, field } => {
            out.push_str("(fieldacc ");
            sexpr_escape(field, out);
            out.push(' ');
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
        // v0.79: array constructors/subscripts/slices.
        Expr::ArrayCtor { elems, nested } => {
            out.push_str("(array ");
            out.push_str(if *nested { "nested" } else { "flat" });
            for e in elems {
                out.push(' ');
                encode_expr_inner(e, out);
            }
            out.push(')');
        }
        Expr::Subscript { array, indices } => {
            out.push_str("(subscript ");
            encode_expr_inner(array, out);
            for i in indices {
                out.push(' ');
                encode_expr_inner(i, out);
            }
            out.push(')');
        }
        Expr::Slice { array, bounds } => {
            out.push_str("(slice ");
            encode_expr_inner(array, out);
            for (lower, upper) in bounds {
                out.push(' ');
                match lower {
                    Some(l) => encode_expr_inner(l, out),
                    None => out.push_str("nil"),
                }
                out.push(' ');
                match upper {
                    Some(u) => encode_expr_inner(u, out),
                    None => out.push_str("nil"),
                }
            }
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
        // v0.68: regex match operators.
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => {
            out.push_str(&format!(
                "(regex {} {} ",
                if *not { 1 } else { 0 },
                if *case_insensitive { 1 } else { 0 }
            ));
            encode_expr_inner(expr, out);
            out.push(' ');
            encode_expr_inner(pattern, out);
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
                // v0.81: `*=` record-image equality.
                CmpOp::ImageEq => "imageeq",
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
        Expr::BitNot(a) => {
            out.push_str("(bitnot ");
            encode_expr_inner(a, out);
            out.push(')');
        }
        Expr::Neg(a) => {
            out.push_str("(neg ");
            encode_expr_inner(a, out);
            out.push(')');
        }
        // v0.55: CASE — `(case <operand|_> (when <k> <r>)... <else|_>)`.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            out.push_str("(case ");
            match operand {
                Some(o) => encode_expr_inner(o, out),
                None => out.push('_'),
            }
            for (k, r) in whens {
                out.push_str(" (when ");
                encode_expr_inner(k, out);
                out.push(' ');
                encode_expr_inner(r, out);
                out.push(')');
            }
            out.push(' ');
            match else_ {
                Some(e) => encode_expr_inner(e, out),
                None => out.push('_'),
            }
            out.push(')');
        }
        Expr::IsNull { expr, neg } => {
            out.push_str(&format!("(isnull {} ", if *neg { 1 } else { 0 }));
            encode_expr_inner(expr, out);
            out.push(')');
        }
        // v0.48: IS [NOT] DISTINCT FROM round-trips like IS NULL.
        Expr::IsDistinctFrom { left, right, neg } => {
            out.push_str(&format!("(isdistinctfrom {} ", if *neg { 1 } else { 0 }));
            encode_expr_inner(left, out);
            out.push(' ');
            encode_expr_inner(right, out);
            out.push(')');
        }
        // Aggregates, subqueries, windows and pre-resolved columns can never
        // appear in a persisted CHECK / DEFAULT (validated at parse time).
        Expr::Agg { .. }
        | Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::UserOp { .. }
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
    /// v0.85: peek whether the next atom (without consuming) equals `want`.
    /// Used for `nil` bounds and `_` placeholders.
    fn peek_atom_is(&mut self, want: &str) -> bool {
        self.ws();
        let s: String = self
            .chars
            .clone()
            .take_while(|c| !c.is_whitespace() && *c != '(' && *c != ')')
            .collect();
        s == want
    }
    /// v0.85: peek whether the upcoming text starts with `prefix`.
    /// Used to distinguish `(when ...)` sublists in CASE encodings.
    fn peek_starts_with(&mut self, prefix: &str) -> bool {
        self.ws();
        let s: String = self.chars.clone().take(prefix.len()).collect();
        s == prefix
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
            // v0.73: whole-row Var.
            "wholerow" => Expr::WholeRow { qual: self.atom()? },
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
                    "bitand" => ArithOp::BitAnd,
                    "bitor" => ArithOp::BitOr,
                    "bitxor" => ArithOp::BitXor,
                    "shl" => ArithOp::Shl,
                    "shr" => ArithOp::Shr,
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
            // v0.68: regex match operators.
            "regex" => {
                let not = self.atom()? == "1";
                let case_insensitive = self.atom()? == "1";
                let x = self.expr()?;
                let p = self.expr()?;
                Expr::Regex {
                    expr: Box::new(x),
                    pattern: Box::new(p),
                    not,
                    case_insensitive,
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
            "bitnot" => {
                let a = self.expr()?;
                Expr::BitNot(Box::new(a))
            }
            "neg" => {
                let a = self.expr()?;
                Expr::Neg(Box::new(a))
            }
            "isnull" => {
                let neg = self.atom()? == "1";
                let x = self.expr()?;
                Expr::IsNull {
                    expr: Box::new(x),
                    neg,
                }
            }
            // v0.48: IS [NOT] DISTINCT FROM.
            "isdistinctfrom" => {
                let neg = self.atom()? == "1";
                let l = self.expr()?;
                let r = self.expr()?;
                Expr::IsDistinctFrom {
                    left: Box::new(l),
                    right: Box::new(r),
                    neg,
                }
            }
            // v0.85: array subscript / slice / constructors, composite
            // field access, row constructors, named casts, CASE — the
            // encoder (`encode_expr_inner`) produces these heads.
            "subscript" => {
                let array = self.expr()?;
                let mut indices = Vec::new();
                loop {
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        break;
                    }
                    indices.push(self.expr()?);
                }
                Expr::Subscript {
                    array: Box::new(array),
                    indices,
                }
            }
            "fieldacc" => {
                let field = self.atom()?;
                let expr = self.expr()?;
                Expr::FieldAccess {
                    expr: Box::new(expr),
                    field,
                }
            }
            "slice" => {
                let array = self.expr()?;
                let mut bounds = Vec::new();
                loop {
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        break;
                    }
                    let low = if self.peek_atom_is("nil") {
                        self.atom()?;
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    let high = if self.peek_atom_is("nil") {
                        self.atom()?;
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    bounds.push((low, high));
                }
                Expr::Slice {
                    array: Box::new(array),
                    bounds,
                }
            }
            "array" => {
                let nested = self.atom()? == "nested";
                let mut elems = Vec::new();
                loop {
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        break;
                    }
                    elems.push(self.expr()?);
                }
                Expr::ArrayCtor { elems, nested }
            }
            "row" => {
                let mut elems = Vec::new();
                loop {
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        break;
                    }
                    elems.push(self.expr()?);
                }
                Expr::Row(elems)
            }
            "castnamed" => {
                let name = self.atom()?;
                let expr = self.expr()?;
                Expr::CastNamed {
                    expr: Box::new(expr),
                    name,
                }
            }
            "case" => {
                let operand = if self.peek_atom_is("_") {
                    self.atom()?;
                    None
                } else {
                    Some(Box::new(self.expr()?))
                };
                let mut whens = Vec::new();
                loop {
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        break;
                    }
                    // Peek: a `(when ...)` sublist vs the final else.
                    // The else is the last element; whens are `(when k r)`.
                    // We distinguish by looking ahead for "(when".
                    if self.peek_starts_with("(when") {
                        self.open()?;
                        let w = self.atom()?;
                        if w != "when" {
                            return Err("expected when in case encoding".into());
                        }
                        let k = self.expr()?;
                        let r = self.expr()?;
                        self.close()?;
                        whens.push((Box::new(k), Box::new(r)));
                    } else {
                        break;
                    }
                }
                let else_ = if self.peek_atom_is("_") {
                    self.atom()?;
                    None
                } else {
                    // Could be `)` already (no else and no whens tail).
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    }
                };
                Expr::Case {
                    operand,
                    whens,
                    else_,
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
                let scale: i32 = self.atom()?.parse().map_err(|_| "bad numeric")?;
                Literal::Numeric(crate::storage::Numeric::new(unscaled, scale))
            }
            "text" => Literal::Text(self.atom()?.into()),
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

pub(crate) fn coltype_by_name(name: &str) -> Result<ColType, String> {
    Ok(match name {
        "integer" | "int" | "int4" => ColType::Int,
        "bigint" | "int8" => ColType::BigInt,
        "smallint" | "int2" => ColType::SmallInt,
        "double precision" | "float8" | "float" => ColType::Float,
        "real" | "float4" => ColType::Float4,
        "numeric" | "decimal" => ColType::Numeric(None),
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
        // v0.76: NOT VALID flag (older checkpoints omit it; the decoder
        // defaults a missing flag to false).
        out.push_str(if c.not_valid { " 1" } else { " 0" });
        // v0.77: constraint kind (`c` = CHECK, `n` = NOT NULL). Older
        // checkpoints omit it; the decoder defaults to CHECK.
        out.push_str(match c.kind {
            CheckKind::Check => " c",
            CheckKind::NotNull => " n",
        });
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
        // v0.76: optional NOT VALID flag (pre-v0.76 checkpoints omit it).
        let not_valid = if sexpr_is_close(&mut p) {
            false
        } else {
            match p.atom()?.as_str() {
                "1" => true,
                "0" => false,
                o => return Err(format!("bad check not-valid flag {}", o)),
            }
        };
        // v0.77: optional kind token (`c` = CHECK, `n` = NOT NULL).
        // Pre-v0.77 checkpoints omit it and decode as CHECK. (A v0.76
        // checkpoint holding an ALTER-added NOT NULL would decode as a
        // plain CHECK — 23514 instead of 23502 on violation after a
        // load; the checkpoint format is not stable across versions,
        // so this corner is documented, not preserved.)
        let kind = if sexpr_is_close(&mut p) {
            CheckKind::Check
        } else {
            match p.atom()?.as_str() {
                "c" => CheckKind::Check,
                "n" => CheckKind::NotNull,
                o => return Err(format!("bad check kind {}", o)),
            }
        };
        p.close()?;
        checks.push(CheckDef {
            name,
            expr,
            not_valid,
            kind,
        });
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

/// v0.85: encode a domain's CHECK list as an s-expr. WAL and checkpoint
/// records carry it as a length-prefixed string (the binary record
/// layer has no expression codec; the s-expr layer does).
pub fn encode_domain_checks(checks: &[CheckDef]) -> String {
    let mut out = String::from("(domainchecks");
    for c in checks {
        out.push('(');
        sexpr_escape(&c.name, &mut out);
        out.push(' ');
        encode_expr_inner(&c.expr, &mut out);
        out.push(')');
    }
    out.push(')');
    out
}

/// v0.85: decode `encode_domain_checks`. Domain checks are always
/// `CheckKind::Check` (23514 on violation).
pub fn decode_domain_checks(s: &str) -> Result<Vec<CheckDef>, String> {
    let mut p = SexprParser::new(s);
    p.open()?;
    if p.atom()? != "domainchecks" {
        return Err("bad domain checks encoding".into());
    }
    let mut checks = Vec::new();
    loop {
        p.ws();
        if p.chars.peek() == Some(&')') {
            p.chars.next();
            break;
        }
        p.open()?;
        let name = p.atom()?;
        let expr = p.expr()?;
        p.close()?;
        checks.push(CheckDef {
            name,
            expr,
            not_valid: false,
            kind: CheckKind::Check,
        });
    }
    p.ws();
    if p.chars.peek().is_some() {
        return Err("trailing data in domain checks encoding".into());
    }
    Ok(checks)
}

/// v0.85: encode a domain DEFAULT as an s-expr (`-` = none), mirroring
/// `encode_default`.
pub fn encode_domain_default(d: &Option<DefaultExpr>) -> String {
    match d {
        Some(dd) => {
            let mut out = String::new();
            encode_default(dd, &mut out);
            out
        }
        None => "-".to_string(),
    }
}

/// v0.85: decode `encode_domain_default`. Reuses the constraint
/// decoder's default forms (`(default-lit ...)`, `(default-nextval
/// ...)`, `(default-expr ...)`).
pub fn decode_domain_default(s: &str) -> Result<Option<DefaultExpr>, String> {
    if s == "-" {
        return Ok(None);
    }
    let mut p = SexprParser::new(s);
    p.open()?;
    let tag = p.atom()?;
    let d = match tag.as_str() {
        "default-lit" => {
            p.open()?;
            if p.atom()? != "lit" {
                return Err("bad domain default-lit head".into());
            }
            let lit = p.literal()?;
            p.close()?;
            DefaultExpr::Lit(lit)
        }
        "default-nextval" => DefaultExpr::Nextval(p.atom()?),
        "default-expr" => DefaultExpr::Expr(p.expr()?),
        _ => return Err(format!("bad domain default encoding: {}", tag)),
    };
    p.close()?;
    Ok(Some(d))
}

// --- v0.70: focused parser tests for PARTITION BY keys -----------------------
#[cfg(test)]
mod partition_by_tests {
    use super::*;

    fn part_def(sql: &str) -> PartitionDef {
        match parse_statement(sql).expect("parses") {
            Stmt::CreateTable { def, .. } => def.partition.expect("has partition def"),
            other => panic!("expected CREATE TABLE, got {:?}", other),
        }
    }

    #[test]
    fn unparenthesized_expression_key() {
        // v0.70: PG accepts any expression as a partition key —
        // `PARTITION BY LIST (lower(a))` (v0.69 required `((lower(a)))`).
        let p = part_def("create table t (a text) partition by list (lower(a))");
        assert_eq!(p.keys.len(), 1);
        match &p.keys[0] {
            PartitionKeyDef::Expr(Expr::Func { name, .. }) => assert_eq!(name, "lower"),
            other => panic!("expected Expr key, got {:?}", other),
        }
    }

    #[test]
    fn bare_column_key() {
        let p = part_def("create table t (a int) partition by list (a)");
        assert_eq!(p.keys.len(), 1);
        assert_eq!(p.keys[0], PartitionKeyDef::Column("a".to_string()));
    }

    #[test]
    fn parenthesized_arithmetic_key() {
        let p = part_def("create table t (a int, b int) partition by range ((a + b))");
        assert_eq!(p.keys.len(), 1);
        assert!(matches!(p.keys[0], PartitionKeyDef::Expr(_)));
    }

    #[test]
    fn opclass_key() {
        // A bare column with an operator class stays a column key.
        let p = part_def("create table t (a int) partition by hash (a int4_ops)");
        assert_eq!(p.keys.len(), 1);
        assert_eq!(p.keys[0], PartitionKeyDef::Column("a".to_string()));
    }

    #[test]
    fn multiple_keys() {
        let p = part_def("create table t (a int, b text) partition by list (a, lower(b))");
        assert_eq!(p.keys.len(), 2);
        assert_eq!(p.keys[0], PartitionKeyDef::Column("a".to_string()));
        assert!(matches!(p.keys[1], PartitionKeyDef::Expr(_)));
    }
}

// v0.72: parser unit tests for the DDL conformance scope (LIKE,
// reloptions, multi-action ALTER). Exec behavior is pinned by
// tests/protocol_test73.py.
#[cfg(test)]
mod v72_ddl_tests {
    use super::*;

    fn create_def(sql: &str) -> TableDef {
        match parse_statement(sql).expect("parses") {
            Stmt::CreateTable { def, .. } => def,
            other => panic!("expected CREATE TABLE, got {:?}", other),
        }
    }

    #[test]
    fn like_bare_clause() {
        let def = create_def("create table t (like src)");
        assert_eq!(def.likes.len(), 1);
        assert_eq!(def.likes[0].source, "src");
        assert!(def.likes[0].options.is_empty());
        assert!(def.columns.is_empty());
    }

    #[test]
    fn like_mixed_with_columns_and_options() {
        let def = create_def(
            "create table t (z int, like src including defaults excluding constraints, like other including all)",
        );
        assert_eq!(def.columns.len(), 1);
        assert_eq!(def.likes.len(), 2);
        assert_eq!(def.likes[0].source, "src");
        assert_eq!(
            def.likes[0].options,
            vec![
                LikeOption {
                    including: true,
                    kind: LikeKind::Defaults
                },
                LikeOption {
                    including: false,
                    kind: LikeKind::Constraints
                }
            ]
        );
        assert_eq!(def.likes[1].source, "other");
        assert_eq!(
            def.likes[1].options,
            vec![LikeOption {
                including: true,
                kind: LikeKind::All
            }]
        );
    }

    #[test]
    fn like_schema_qualified_source() {
        let def = create_def("create table t (like public.src)");
        assert_eq!(def.likes[0].source, "public.src");
    }

    #[test]
    fn like_including_excluding_compression() {
        let def = create_def(
            "create table t (like src including compression, like o2 excluding compression)",
        );
        assert_eq!(def.likes.len(), 2);
        assert_eq!(
            def.likes[0].options,
            vec![LikeOption {
                including: true,
                kind: LikeKind::Compression
            }]
        );
        assert_eq!(
            def.likes[1].options,
            vec![LikeOption {
                including: false,
                kind: LikeKind::Compression
            }]
        );
    }

    #[test]
    fn reloptions_fillfactor() {
        let def =
            create_def("create table t (a int) with (fillfactor = 10, autovacuum_enabled = true)");
        assert_eq!(
            def.reloptions,
            vec![
                ("fillfactor".to_string(), "10".to_string()),
                ("autovacuum_enabled".to_string(), "true".to_string())
            ]
        );
    }

    #[test]
    fn alter_multi_action() {
        match parse_statement("alter table t add a int, add b text").expect("parses") {
            Stmt::AlterTable { name, actions, .. } => {
                assert_eq!(name, "t");
                assert_eq!(actions.len(), 2);
                assert!(matches!(actions[0], AlterAction::AddColumn { .. }));
                assert!(matches!(actions[1], AlterAction::AddColumn { .. }));
            }
            other => panic!("expected ALTER TABLE, got {:?}", other),
        }
    }

    #[test]
    fn alter_single_action_still_vec_of_one() {
        match parse_statement("alter table t add a int").expect("parses") {
            Stmt::AlterTable { actions, .. } => assert_eq!(actions.len(), 1),
            other => panic!("expected ALTER TABLE, got {:?}", other),
        }
    }
}
