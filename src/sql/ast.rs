// v1.78 mechanical split: moved verbatim from src/sql.rs (38-5702).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

#[derive(Debug)]
pub struct SqlError {
    pub message: String,
    /// SQLSTATE for this parse error. Syntax errors are 42601; undefined
    /// functions / wrong arity are 42883 (like Postgres' parser).
    pub code: &'static str,
}

pub(crate) fn err(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "42601",
    }
}

/// v1.30: outcome of `parse_ordered_set_call`. `NotWithinGroup` means
/// the call is not an ordered-set aggregate after all (only possible
/// for `rank` / `dense_rank`, which double as window functions) — the
/// caller rewinds and parses it through the generic path instead.
pub(crate) enum ParseOsError {
    NotWithinGroup,
    Sql(SqlError),
}

/// A parse-time 42883 (undefined function), like Postgres.
pub(crate) fn err_undefined(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "42883",
    }
}

/// A parse-time 42701 (duplicate_column), like Postgres.
pub(crate) fn err_duplicate(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "42701",
    }
}

/// A parse-time 22023 (invalid_parameter_value), like PG's
/// anychar_typmodin for bad character-type length modifiers.
pub(crate) fn err_typmod(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "22023",
    }
}

/// v1.45: a parse-time 22023 (invalid_parameter_value), like PG19's
/// explain_state.c rejecting bad EXPLAIN option values or option
/// combinations (e.g. `EXPLAIN option WAL requires ANALYZE`).
pub(crate) fn err_invalid_param(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "22023",
    }
}

/// v1.39: a parse-time 22P02 (invalid_text_representation), like PG19's
/// `bit_in` rejecting a bad digit in an `x'...'`/`b'...'` literal.
pub(crate) fn err_invalid_text(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "22P02",
    }
}

/// v0.28: parse the digits of a PG 16+ non-decimal integer literal (the
/// `0x`/`0o`/`0b` prefix is already stripped). Underscores between digits
/// are ignored, like Postgres. Returns None when there are no digits or
/// the value overflows i64.
pub(crate) fn parse_int_radix(digits: &str, radix: u32) -> Option<i64> {
    let clean: String = digits.chars().filter(|&c| c != '_').collect();
    if clean.is_empty() {
        return None;
    }
    i64::from_str_radix(&clean, radix).ok()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Token {
    Ident(String), // folded to lowercase unless double-quoted
    /// v0.36: double-quoted identifier, kept verbatim (no case folding).
    /// Quoted identifiers are never keywords, and quoted `"char"` is
    /// PG's one-byte "char" type (OID 18), not `character(1)`.
    QIdent(String),
    Number(String),
    Str(String),
    UStr(String),   // v0.19: U&'...' raw content (UESCAPE handled by parser)
    UIdent(String), // v0.24: U&"..." raw identifier content (UESCAPE handled by parser)
    // v1.39: `x'...'`/`b'...'` bit-string literal (PG19 scan.l xhstart):
    // the char is the normalized lowercase marker ('x' or 'b'), the
    // String is the raw digit content (validated by the parser with
    // `bit_in` semantics, not the lexer).
    BitStr(char, String),
    Param(u32), // $N parameter placeholder, 1-based
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
    FatArrow,      // v0.95: `=>` named-argument operator (PG19)
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
pub(crate) fn parse_single_quoted(chars: &[char], i: &mut usize) -> Result<String, SqlError> {
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
pub(crate) fn unescape_e_string(s: &str) -> Result<String, SqlError> {
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
pub(crate) fn is_valid_uescape(c: char) -> bool {
    !(c.is_ascii_hexdigit() || c == '+' || c == '\'' || c == '"' || c.is_whitespace())
}

pub(crate) fn decode_ustr(s: &str, escape: char) -> Option<String> {
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
pub(crate) fn consume_dec_digits(chars: &[char], i: &mut usize) {
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

pub(crate) fn tokenize(input: &str) -> Result<Vec<Token>, SqlError> {
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
        // v1.39: `x'...'`/`b'...'` bit-string literal prefixes (PG19
        // scan.l xhstart: no space allowed between the prefix and the
        // quote, and the scanner prepends the marker to the literal).
        // Validation happens in the parser (`bit_in` semantics); the
        // lexer only captures the raw digits.
        if (c == 'x' || c == 'X' || c == 'b' || c == 'B')
            && i + 1 < chars.len()
            && chars[i + 1] == '\''
        {
            let marker = c.to_ascii_lowercase();
            i += 1; // consume the prefix, now at the quote
            let s = parse_single_quoted(&chars, &mut i)?;
            toks.push(Token::BitStr(marker, s));
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
                // v0.95: `=>` is the named-argument operator (PG19); a
                // bare `=` is equality.
                if i + 1 < chars.len() && chars[i + 1] == '>' {
                    toks.push(Token::FatArrow);
                    i += 2;
                } else {
                    toks.push(Token::Eq);
                    i += 1;
                }
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
    // v1.39: a validated `x'...'`/`b'...'` bit-string literal (PG19
    // `bit_in`); the marker's case was normalized by the lexer.
    BitString(crate::storage::BitString),
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
            Literal::BitString(_) => "bit", // v1.39
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
            Literal::BitString(_) => ColType::Bit, // v1.39
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
            Literal::BitString(b) => Value::BitString(b), // v1.39
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

/// v1.30: PG19 ordered-set aggregates (`aggkind = 'o'` / `'h'` in
/// pg_aggregate.dat), called with `WITHIN GROUP (ORDER BY ...)`
/// (PG19 gram.y `within_group_clause`). `Rank` / `DenseRank` /
/// `PercentRank` / `CumeDist` are the hypothetical-set aggregates;
/// the window functions of the same names are the separate
/// `WindowFunc` variants (disambiguated by the WITHIN GROUP keyword).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderedSetAgg {
    PercentileCont,
    PercentileDisc,
    Mode,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
}

impl OrderedSetAgg {
    pub fn name(&self) -> &'static str {
        match self {
            OrderedSetAgg::PercentileCont => "percentile_cont",
            OrderedSetAgg::PercentileDisc => "percentile_disc",
            OrderedSetAgg::Mode => "mode",
            OrderedSetAgg::Rank => "rank",
            OrderedSetAgg::DenseRank => "dense_rank",
            OrderedSetAgg::PercentRank => "percent_rank",
            OrderedSetAgg::CumeDist => "cume_dist",
        }
    }

    /// PG19 `aggkind`: `'h'` for the hypothetical-set aggregates,
    /// `'o'` for the plain ordered-set ones (pg_aggregate.dat).
    pub fn is_hypothetical(&self) -> bool {
        matches!(
            self,
            OrderedSetAgg::Rank
                | OrderedSetAgg::DenseRank
                | OrderedSetAgg::PercentRank
                | OrderedSetAgg::CumeDist
        )
    }
}

/// v1.30: is `name` one of PG19's ordered-set aggregate names
/// (pg_aggregate.dat `aggkind = 'o'` / `'h'`)?
pub fn is_ordered_set_agg_name(name: &str) -> bool {
    matches!(
        name,
        "percentile_cont"
            | "percentile_disc"
            | "mode"
            | "rank"
            | "dense_rank"
            | "percent_rank"
            | "cume_dist"
    )
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
        /// v0.93: func-style cast spelling (`float8(x)`): PG19 names the
        /// output column after the type name as written. `None` for
        /// `x::type` / `CAST(x AS type)` syntax.
        written: Option<String>,
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
    /// v0.95: `name => expr` named function argument (PG19). The parser
    /// produces this inside `Func.args`; the executor resolves it against
    /// the function signature before evaluation.
    NamedArg {
        name: String,
        expr: Box<Expr>,
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
        /// v1.29: `FILTER (WHERE ...)` (PG19 gram.y `filter_clause`):
        /// only input rows where this evaluates to TRUE feed the
        /// aggregate's transition. Groups are still formed from all
        /// rows; evaluated per input row in row scope.
        filter: Option<Box<Expr>>,
    },
    /// v1.30: PG19 ordered-set aggregate, e.g.
    /// `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)` or
    /// `rank(5) WITHIN GROUP (ORDER BY x)` (PG19 gram.y
    /// `within_group_clause`). `direct_args` are the parenthesized
    /// arguments, evaluated once per group (PG19 nodeAgg.c evaluates
    /// them against the group's representative input tuple); the
    /// WITHIN GROUP sort keys are the aggregated args, sorted (with
    /// the terms' ASC/DESC and NULLS ordering) before the final
    /// function runs. `filter` is the v1.29 `FILTER (WHERE ...)`
    /// clause, applied to input rows before the sort.
    WithinGroup {
        func: OrderedSetAgg,
        direct_args: Vec<Expr>,
        within_order_by: Vec<OrderTerm>,
        filter: Option<Box<Expr>>,
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
        /// v1.29: `FILTER (WHERE ...)` on a windowed aggregate (PG19
        /// parse_func.c keeps `wfunc->aggfilter`; only meaningful when
        /// `func` is `WindowFunc::Agg`).
        filter: Option<Box<Expr>>,
        /// v1.31: PG19 `opt_window_exclusion_clause` (`EXCLUDE ...`
        /// after the frame extent). Participates in window identity
        /// like `filter`.
        exclusion: FrameExclusion,
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
/// v1.31: `Groups` (PG19 gram.y `GROUPS frame_extent`): bounds count
/// peer groups (rows indistinguishable by the window ORDER BY), not
/// physical rows.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowFrame {
    Default,
    Rows { start: FrameBound, end: FrameBound },
    Range { start: FrameBound, end: FrameBound },
    Groups { start: FrameBound, end: FrameBound },
}

/// v1.31: PG19 `opt_window_exclusion_clause` — `EXCLUDE CURRENT ROW` /
/// `EXCLUDE GROUP` / `EXCLUDE TIES` / `EXCLUDE NO OTHERS` after the frame
/// extent. Absent and `EXCLUDE NO OTHERS` are both `NoOthers` (PG folds
/// them to "no exclusion bits").
#[derive(Clone, Debug, PartialEq, Default)]
pub enum FrameExclusion {
    #[default]
    NoOthers,
    CurrentRow,
    Group,
    Ties,
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
        /// v0.96: `FROM ONLY tbl` — scan just the named table, excluding
        /// inheritance children (PG19; default scans include them).
        only: bool,
    },
    /// `(SELECT ...) [AS] alias` — the alias is required, like Postgres.
    /// v0.23: `[(cols)]` column aliases rename the subquery's output
    /// columns positionally (previously parsed but discarded).
    /// v1.10: `LATERAL (SELECT ...)` — the subquery may reference
    /// FROM items to its left and is evaluated once per left row
    /// (PG19 `LATERAL_P select_with_parens`, which also covers
    /// `LATERAL (VALUES ...)`).
    Derived {
        sub: Box<SelectStmt>,
        alias: String,
        col_aliases: Vec<String>,
        lateral: bool,
    },
    /// v0.14: `(VALUES (e, ...) [, ...]) [AS] alias` — PG names the
    /// columns `column1`, `column2`, ... when no column aliases are given.
    /// v0.21: `[(col, ...)]` column aliases.
    /// v1.10: `LATERAL (VALUES ...)` — row expressions may reference
    /// FROM items to the left (PG19 `select_with_parens` covers
    /// `VALUES`; PG's own regress suite uses this shape).
    Values {
        rows: Vec<Vec<Expr>>,
        alias: String,
        col_aliases: Vec<String>,
        lateral: bool,
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
    /// v1.68: PG19 `JOIN_ANTI` (primnodes.h) — the anti-join produced by
    /// `convert_ANY_sublink_to_join` / `convert_EXISTS_sublink_to_join`
    /// (subselect.c) for `NOT IN` / `NOT EXISTS`. EXPLAIN renders it
    /// interpolated into the node label (`Hash Anti Join`, explain.c
    /// `ExplainNode`). Never produced by the SQL parser (there is no
    /// `ANTI JOIN` syntax); only by the EXPLAIN planner.
    Anti,
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
    /// v1.14: `FOR UPDATE OF tbl [, ...]` — the locking targets (PG19).
    /// Empty when absent or for bare `FOR UPDATE` (which locks all tables).
    pub for_update_of: Vec<String>,
    /// v0.44: set-operation root when this statement is the carrier of a
    /// `UNION` / `INTERSECT` / `EXCEPT` query. When `Some`, the fields
    /// above are empty/ignored and the query is `set_op`'s branches.
    /// `None` for plain SELECTs (zero behavior change).
    pub set_op: Option<Box<SetOpRoot>>,
    /// v1.54: PG19's dead rtable entries from `pull_up_simple_subquery`
    /// (prepjointree.c): a pulled-up subquery's RTE stays in the rtable
    /// (with `subquery = NULL`), so `es->rtable_size` — the EXPLAIN
    /// `useprefix` rule — counts it. The parser always sets 0; the
    /// EXPLAIN-path pullup sets the number of spliced Deriveds.
    /// Consulted only by PG-text rendering (`pg_rtable_size` call
    /// sites); never by execution.
    pub dead_rtes: usize,
}

/// v1.54: `AS [NOT] MATERIALIZED` CTE hint (PG12+, PG19 gram.y
/// `materialized` opt). Recorded by the parser; the planner's CTE-inlining
/// gate (`SS_process_ctes`) honors it. `Default` = no hint given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CteMaterialize {
    Default,
    Materialized,
    NotMaterialized,
}

/// v0.10: one Common Table Expression.
#[derive(Clone, Debug, PartialEq)]
pub struct CteDef {
    pub name: String,
    pub col_aliases: Vec<String>,
    pub body: CteBody,
    pub recursive: bool,
    /// v1.54: the `AS [NOT] MATERIALIZED` hint (`Default` when absent).
    pub materialized: CteMaterialize,
}

/// v0.44: an empty `SelectStmt` shell, used as the carrier of a
/// set-operation root (its query fields stay empty/ignored).
pub(crate) fn empty_select() -> SelectStmt {
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
        for_update_of: Vec::new(),
        set_op: None,
        dead_rtes: 0,
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
#[derive(Clone, Debug)]
pub enum CteBody {
    Simple(SelectStmt),
    Union {
        left: Box<SelectStmt>,
        right: Box<SelectStmt>,
        all: bool,
    },
    // v1.39: a data-modifying CTE body — `(INSERT/UPDATE/DELETE ...
    // [RETURNING ...])` (PG19 parse_cte.c). The DML runs when the CTE is
    // materialized; its RETURNING rows become the CTE's rows. An empty
    // RETURNING list means zero columns.
    Dml(Box<Stmt>),
}

// v1.39: manual PartialEq — `Stmt` does not implement PartialEq (several
// of its field types don't), so the derived impl cannot cover
// `CteBody::Dml`. DML bodies are never compared in this engine; two
// DML bodies compare unequal.
impl PartialEq for CteBody {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CteBody::Simple(a), CteBody::Simple(b)) => a == b,
            (
                CteBody::Union {
                    left: a,
                    right: b,
                    all: c,
                },
                CteBody::Union {
                    left: d,
                    right: e,
                    all: f,
                },
            ) => a == d && b == e && c == f,
            _ => false,
        }
    }
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
    /// v0.96: table-constraint `NOT NULL` columns that named no local
    /// column (legal only with `INHERITS`); applied to the merged
    /// column list at exec, 42703 when still missing.
    pub deferred_not_null: Vec<String>,
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
    /// v0.96: `ALTER TABLE child INHERIT parent` (PG19 table inheritance).
    /// The parent must exist; every parent column must already exist in
    /// the child with an exactly matching type, and parent NOT NULL
    /// columns must be NOT NULL in the child (42804 otherwise). The
    /// link is recorded; columns are never added or reordered.
    Inherit {
        parent: String,
    },
    /// v0.96: `ALTER TABLE child NO INHERIT parent` — drops the
    /// inheritance link only; the child's columns and data are kept.
    /// A missing link is 42P01, like PostgreSQL.
    NoInherit {
        parent: String,
    },
}

/// v0.9: CREATE / ALTER SEQUENCE options. `None` = keep current value
/// (ALTER) or the Postgres default (CREATE).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SequenceOpts {
    /// v0.99: explicit `AS smallint | int | bigint` (PG19; default bigint).
    pub seq_type: Option<SeqType>,
    pub start: Option<i64>,
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    /// v0.99: `NO MINVALUE` / `NO MAXVALUE` — reset to the type default
    /// (PG19 init_params). Distinct from `None` (keep current on ALTER).
    pub reset_min: bool,
    pub reset_max: bool,
    pub cycle: Option<bool>,
    pub restart: Option<i64>,
    /// v0.98: `CACHE n` (Postgres default 1).
    pub cache: Option<i64>,
    /// v0.98: `OWNED BY table.col` / `OWNED BY NONE`. `None` =
    /// unspecified (ALTER keeps the current owner link).
    pub owned_by: Option<OwnedBySpec>,
}

/// v0.99: explicit sequence data type (`AS smallint | int | bigint`,
/// PG19; anything else is 22023 "sequence type must be smallint,
/// integer, or bigint").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqType {
    SmallInt,
    Integer,
    BigInt,
}

impl SeqType {
    pub fn pg_name(self) -> &'static str {
        match self {
            SeqType::SmallInt => "smallint",
            SeqType::Integer => "integer",
            SeqType::BigInt => "bigint",
        }
    }
    pub fn min_value(self) -> i64 {
        match self {
            SeqType::SmallInt => i64::from(i16::MIN),
            SeqType::Integer => i64::from(i32::MIN),
            SeqType::BigInt => i64::MIN,
        }
    }
    pub fn max_value(self) -> i64 {
        match self {
            SeqType::SmallInt => i64::from(i16::MAX),
            SeqType::Integer => i64::from(i32::MAX),
            SeqType::BigInt => i64::MAX,
        }
    }
}

/// v0.98: `OWNED BY` target in CREATE/ALTER SEQUENCE (PG19
/// SequenceOptions / AlterSeqStmt).
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedBySpec {
    Table { table: String, column: String },
    None_,
}

impl SequenceOpts {
    /// Bare `RESTART` (no WITH value) resets to the sequence's start.
    pub const RESTART_SENTINEL: i64 = i64::MIN;
}

// ---------------------------------------------------------------------------
// v0.9: parser-internal intermediate forms for CREATE TABLE.
// ---------------------------------------------------------------------------

/// One item inside `CREATE TABLE (...)`.
pub(crate) enum TableItem {
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

pub(crate) struct ParsedColDef {
    pub(crate) name: String,
    pub(crate) col_type: ColType,
    /// v0.81: named composite type for `col_type == ColType::Composite`
    /// (`a t_rec`); resolved against the type catalog at execution time.
    pub(crate) composite_name: Option<String>,
    /// v0.65: serial pseudo-type marker, detected from the raw type
    /// name before it is resolved to a ColType.
    pub(crate) serial: Option<SerialKind>,
    /// v0.41: PG19 `opt_column_compression`: `COMPRESSION method` /
    /// `COMPRESSION DEFAULT` right after the type name. Raw name;
    /// validated in exec (`default` = no explicit method).
    pub(crate) compression: Option<String>,
    pub(crate) cons: Vec<ColCon>,
}

pub(crate) enum ColCon {
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

pub(crate) enum ParsedTableCon {
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

pub(crate) struct ParsedFkTail {
    pub(crate) ref_table: String,
    pub(crate) ref_cols: Vec<String>,
    pub(crate) on_delete: FkAction,
    pub(crate) on_update: FkAction,
}

/// Classify a DEFAULT expression into its stored form.
pub(crate) fn classify_default(e: Expr) -> Result<DefaultExpr, SqlError> {
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
            deferred_not_null: Vec::new(),
        }
    }
}

/// Resolve a parsed CREATE TABLE item list into a `TableDef`, assigning
/// Postgres-style automatic constraint names.

pub(crate) fn def_col_exists(def: &TableDef, n: &str) -> bool {
    def.columns.iter().any(|(c, _)| c == n)
}

pub(crate) fn def_constraint_name_exists(def: &TableDef, cname: &str) -> bool {
    def.uniques.iter().any(|u| u.name == cname)
        || def.checks.iter().any(|c| c.name == cname)
        || def.fks.iter().any(|f| f.name == cname)
        || def.pkey.as_ref().map(|p| p.name == cname).unwrap_or(false)
}

pub(crate) fn def_add_pkey(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    cols: &[String],
    // v0.96: with `INHERITS`, columns may come from the parents; the
    // existence check and the NOT NULL marking are deferred to exec,
    // which validates against the merged column list.
    defer_col_check: bool,
) -> Result<(), SqlError> {
    if !defer_col_check {
        for c in cols {
            if !def_col_exists(def, c) {
                return Err(err(format!("column \"{}\" does not exist", c)));
            }
        }
    }
    if def.pkey.is_some() {
        return Err(err("multiple primary keys for table".to_string()));
    }
    let cname = name.unwrap_or_else(|| format!("{}_pkey", table));
    if def_constraint_name_exists(def, &cname) {
        return Err(err(format!("constraint \"{}\" already exists", cname)));
    }
    if !defer_col_check {
        for c in cols {
            let i = def.columns.iter().position(|(n, _)| n == c).unwrap();
            def.not_null[i] = true;
        }
    }
    def.pkey = Some(UniqueDef {
        name: cname,
        cols: cols.to_vec(),
    });
    Ok(())
}

pub(crate) fn def_add_unique(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    cols: &[String],
    col: &str,
    // v0.96: with `INHERITS`, columns may come from the parents; the
    // existence check is deferred to exec (see def_add_pkey).
    defer_col_check: bool,
) -> Result<(), SqlError> {
    if !defer_col_check {
        for c in cols {
            if !def_col_exists(def, c) {
                return Err(err(format!("column \"{}\" does not exist", c)));
            }
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

pub(crate) fn def_add_check(
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

pub(crate) fn def_add_fk(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    cols: &[String],
    tail: ParsedFkTail,
    // v0.96: with `INHERITS`, columns may come from the parents; the
    // existence check is deferred to exec (see def_add_pkey).
    defer_col_check: bool,
) -> Result<(), SqlError> {
    if !defer_col_check {
        for c in cols {
            if !def_col_exists(def, c) {
                return Err(err(format!("column \"{}\" does not exist", c)));
            }
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

/// v0.96: `inherits` (already parsed) relaxes two rules when non-empty:
/// a column list with no local columns is legal (parents supply them),
/// and table-constraint column references are validated against the
/// merged column list at exec instead of here.
pub(crate) fn build_table_def(
    table: &str,
    items: Vec<TableItem>,
    inherits: &[String],
) -> Result<TableDef, SqlError> {
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
    if def.columns.is_empty() && !has_like && !items.is_empty() && inherits.is_empty() {
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
                            false,
                        )?,
                        ColCon::PKey(n) => def_add_pkey(
                            table,
                            &mut def,
                            n.clone(),
                            std::slice::from_ref(&c.name),
                            false,
                        )?,
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
                            false,
                        )?,
                    }
                }
            }
            TableItem::TableCon(tc) => match tc {
                ParsedTableCon::PKey(n, cols) => {
                    def_add_pkey(table, &mut def, n.clone(), cols, !inherits.is_empty())?
                }
                ParsedTableCon::Unique(n, cols) => {
                    let first = cols[0].clone();
                    def_add_unique(
                        table,
                        &mut def,
                        n.clone(),
                        cols,
                        &first,
                        !inherits.is_empty(),
                    )?
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
                    } else if inherits.is_empty() {
                        return Err(err(format!(
                            "syntax error: column \"{}\" of relation \"{}\" does not exist",
                            col, table
                        )));
                    } else {
                        // v0.96: with INHERITS the column may come from a
                        // parent; the NOT NULL is applied to the merged
                        // columns at exec.
                        def.deferred_not_null.push(col.clone());
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
                    !inherits.is_empty(),
                )?,
            },
        }
    }
    Ok(def)
}

/// Collect `(qualifier, name)` of every column reference in an expression.
pub(crate) fn collect_col_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => collect_col_refs(expr, out),
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
        | Expr::WithinGroup { .. }
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

/// v0.86: function languages we can execute. `plpgsql` is accepted only
/// for the bounded single-`RETURN` subset (v0.97): a body of the form
/// `BEGIN RETURN <expr>; END` is desugared to `SELECT <expr>` at CREATE
/// time (see `desugar_plpgsql_body`); anything else in plpgsql is an
/// honest 0A000. v1.32: unknown language names are no longer rejected by
/// the parser — the executor resolves them and raises 42883
/// (`language "%s" does not exist`, like PG19's proclang.c
/// `get_language_oid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncLang {
    Sql,
    Internal,
    /// v0.97: bounded plpgsql (single RETURN statement bodies only).
    Plpgsql,
}

/// v0.97: one `ALTER DOMAIN` action (PG19 AlterDomainStmt, bounded to
/// the domain-constraint forms; OWNER/RENAME/SET SCHEMA stay
/// unsupported). `AddConstraint.name` is `None` when the statement
/// omitted `CONSTRAINT name` — the executor auto-names it
/// `<domain>_check[N]` like CREATE DOMAIN does.
#[derive(Clone, Debug)]
pub enum AlterDomainAction {
    AddConstraint { name: Option<String>, expr: Expr },
    DropConstraint { name: String, if_exists: bool },
    SetNotNull,
    DropNotNull,
    SetDefault(DefaultExpr),
    DropDefault,
}

/// v0.97: desugar a bounded-plpgsql function body to a SQL SELECT body.
///
/// Accepts (case-insensitively, whitespace-tolerant) exactly:
/// `BEGIN RETURN <expr>; END` — a single RETURN statement. `<expr>` must
/// not contain a semicolon. Returns `SELECT <expr>` preserving the
/// expression's original text, or an error describing why the body is
/// outside the supported subset (the caller maps this to 0A000). This
/// is deliberately not a plpgsql parser: real plpgsql (variables,
/// control flow, multi-statement bodies, EXCEPTION blocks, ...) stays
/// unsupported.
pub fn desugar_plpgsql_body(body: &str) -> Result<String, String> {
    let unsupported = || {
        format!(
            "only single-RETURN plpgsql bodies (BEGIN RETURN expr; END) are supported, got: {}",
            body.chars().take(60).collect::<String>()
        )
    };
    /// Strip an ASCII keyword (case-insensitive) from the front of `s`,
    /// requiring a word boundary after it. Returns the remainder.
    fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
        let rest = s.get(..kw.len())?;
        if !rest.eq_ignore_ascii_case(kw) {
            return None;
        }
        let after = s.get(kw.len()..)?;
        // Word boundary: next char must not be ident-continue. The
        // keyword itself may be followed by end-of-string.
        if let Some(c) = after.chars().next() {
            if c.is_alphanumeric() || c == '_' {
                return None;
            }
        }
        Some(after)
    }
    let t = body.trim();
    let t = match t.strip_suffix(';') {
        Some(s) => s.trim_end(),
        None => t,
    };
    let inner = strip_kw(t, "begin").ok_or_else(unsupported)?;
    // The body must end with the END keyword (word boundary before it).
    // Find it by scanning from the end: strip trailing whitespace, then
    // require the last 3 chars to be "end" with a boundary before.
    let inner = inner.trim_end();
    if inner.len() < 3 || !inner[inner.len() - 3..].eq_ignore_ascii_case("end") {
        return Err(unsupported());
    }
    let before_end = &inner[..inner.len() - 3];
    if let Some(c) = before_end.chars().last() {
        if c.is_alphanumeric() || c == '_' {
            return Err(unsupported());
        }
    }
    let stmts = before_end.trim();
    let expr = strip_kw(stmts, "return").ok_or_else(unsupported)?.trim();
    let expr = match expr.strip_suffix(';') {
        Some(s) => s.trim_end(),
        None => expr,
    };
    if expr.is_empty() || expr.contains(';') {
        return Err(unsupported());
    }
    Ok(format!("SELECT {}", expr))
}

// ---------------------------------------------------------------------------
// v1.01: bounded PL/pgSQL statement sequences + EXCEPTION blocks.
//
// PG19 grounding:
// - Grammar: `pl_block` / `exception_sect` / `proc_exceptions` /
//   `proc_conditions` (`src/pl/plpgsql/src/pl_gram.y`).
// - Condition name -> SQLSTATE: `plpgsql_parse_err_condition`
//   (`pl_comp.c`) over `exception_label_map` (generated `plerrcodes.h`).
// - Trap semantics: `exec_stmt_block` / `exception_matches_conditions`
//   (`pl_exec.c`): the first WHEN clause whose condition list matches
//   wins; exact SQLSTATE match, category match for `...000` conditions,
//   OTHERS matches everything except query_canceled (57014) and
//   assert_failure (P0004); the trapped error aborts the block's
//   subtransaction; errors raised inside a handler propagate untrapped.
// - Missing trailing RETURN: `add_dummy_return` (`pl_comp.c`) appends an
//   implicit `RETURN NULL` outside the exception block.
//
// Deliberate deviations from PG19:
// - No subtransactions: a trapped error does NOT roll back the effects
//   of statements that ran before it in the body (rustgres has no
//   subtransaction machinery; ANALYZE — the only supported utility
//   statement — has no transactional effects to roll back anyway).
// - No variables: no DECLARE section, no assignments, and no
//   SQLSTATE/SQLERRM magic variables inside handlers.
// - Statement subset: each statement is `RETURN <expr>` or a supported
//   utility statement (currently only ANALYZE). Anything else is an
//   honest 0A000 at CREATE; a malformed RETURN expression is 42601; an
//   unknown condition name is 42704 (`unrecognized exception condition`,
//   like PG's ERRCODE_UNDEFINED_OBJECT).
// ---------------------------------------------------------------------------

/// v1.01: one statement of a bounded PL/pgSQL body.
#[derive(Clone, Debug)]
pub enum PlpgsqlStmt {
    /// `RETURN <expr>`: stored as the parsed `SELECT <expr>` so the
    /// executor reuses `subst_params` + `run_select` verbatim. Always
    /// `Stmt::Select` (enforced at parse).
    Return(Stmt),
    /// A supported utility statement, parsed at CREATE (currently only
    /// `Stmt::Analyze`); executed for side effects, results discarded.
    Utility(Stmt),
    /// v1.02: `RAISE NOTICE|EXCEPTION '<format>' [, <expr> ...]`.
    /// Reuses the v1.00 trigger-body [`RaiseLevel`] enum and the shared
    /// `%-format` evaluator (`format_raise_message` in exec.rs) — the
    /// parse shape mirrors the trigger-body RAISE. Unsupported levels
    /// (DEBUG/LOG/INFO/WARNING) are rejected at parse with 0A000, like
    /// trigger bodies.
    Raise {
        level: RaiseLevel,
        format: String,
        args: Vec<Expr>,
    },
    /// v1.03: `var := <expr>` — scalar assignment to a DECLAREd variable.
    /// Stored as the parsed `SELECT <expr>` (always `Stmt::Select`,
    /// enforced at parse) plus the variable name; the executor evaluates
    /// it like a RETURN and writes the value into the variable's slot.
    Assign { var: String, select: Stmt },
    /// v1.03: `FOR var IN <query> LOOP <stmts> END LOOP` — the loop
    /// variable must be DECLAREd. The query is a row-producing `SELECT`
    /// or `EXPLAIN` (always parsed, enforced at parse); each row binds
    /// the variable to the row's first column, coerced to its declared
    /// type. Only `Assign` and `ReturnNext` are allowed in the body.
    ForQuery {
        var: String,
        query: Stmt,
        body: Vec<PlpgsqlStmt>,
    },
    /// v1.03: `RETURN NEXT <expr>` — only valid in `RETURNS SETOF`
    /// functions (enforced at parse). Stored as the parsed
    /// `SELECT <expr>` (always `Stmt::Select`); the executor appends the
    /// row to the result set and continues.
    ReturnNext(Stmt),
}

/// v1.01: one `WHEN <conditions> THEN <statements>` clause.
#[derive(Clone, Debug)]
pub struct PlpgsqlHandler {
    /// Trapped SQLSTATEs in source order (one condition name may expand
    /// to several codes, like PG's `PLpgSQL_condition` list). The
    /// pseudo-condition OTHERS is stored as the sentinel
    /// `PLPGSQL_OTHERS_SENTINEL`.
    pub sqlstates: Vec<String>,
    pub stmts: Vec<PlpgsqlStmt>,
}

/// v1.01: a parsed bounded PL/pgSQL body:
/// `[DECLARE <decls>] BEGIN <stmts> [EXCEPTION <handlers>] END`.
#[derive(Clone, Debug)]
pub struct PlpgsqlBody {
    /// v1.03: scalar variable declarations from an optional `DECLARE`
    /// section (empty when the body has none).
    pub decls: Vec<PlpgsqlDecl>,
    pub stmts: Vec<PlpgsqlStmt>,
    pub handlers: Vec<PlpgsqlHandler>,
}

/// v1.03: one scalar variable declaration (`name type`) from a plpgsql
/// `DECLARE` section. No initializers, no row/record types.
#[derive(Clone, Debug)]
pub struct PlpgsqlDecl {
    pub name: String,
    pub type_name: String,
}

/// v1.01: sentinel for the OTHERS pseudo-condition (never a real
/// SQLSTATE; PG keeps it as the `PLPGSQL_OTHERS` enum value in
/// `pl_comp.c`).
pub(crate) const PLPGSQL_OTHERS_SENTINEL: &str = "OTHERS";

pub(crate) static PLPGSQL_CONDITIONS: &[(&str, &str)] = &[
    ("active_sql_transaction", "25001"),
    ("admin_shutdown", "57P01"),
    ("ambiguous_alias", "42P09"),
    ("ambiguous_column", "42702"),
    ("ambiguous_function", "42725"),
    ("ambiguous_parameter", "42P08"),
    ("array_subscript_error", "2202E"),
    ("assert_failure", "P0004"),
    ("bad_copy_file_format", "22P04"),
    ("branch_transaction_already_active", "25002"),
    ("cannot_coerce", "42846"),
    ("cannot_connect_now", "57P03"),
    ("cant_change_runtime_param", "55P02"),
    ("cardinality_violation", "21000"),
    ("case_not_found", "20000"),
    ("character_not_in_repertoire", "22021"),
    ("check_violation", "23514"),
    ("collation_mismatch", "42P21"),
    ("config_file_error", "F0000"),
    ("configuration_limit_exceeded", "53400"),
    ("connection_does_not_exist", "08003"),
    ("connection_exception", "08000"),
    ("connection_failure", "08006"),
    ("containing_sql_not_permitted", "38001"),
    ("crash_shutdown", "57P02"),
    ("data_corrupted", "XX001"),
    ("data_exception", "22000"),
    ("database_dropped", "57P04"),
    ("datatype_mismatch", "42804"),
    ("datetime_field_overflow", "22008"),
    ("deadlock_detected", "40P01"),
    ("dependent_objects_still_exist", "2BP01"),
    ("dependent_privilege_descriptors_still_exist", "2B000"),
    ("diagnostics_exception", "0Z000"),
    ("disk_full", "53100"),
    ("division_by_zero", "22012"),
    ("duplicate_alias", "42712"),
    ("duplicate_column", "42701"),
    ("duplicate_cursor", "42P03"),
    ("duplicate_database", "42P04"),
    ("duplicate_file", "58P02"),
    ("duplicate_function", "42723"),
    ("duplicate_json_object_key_value", "22030"),
    ("duplicate_object", "42710"),
    ("duplicate_prepared_statement", "42P05"),
    ("duplicate_schema", "42P06"),
    ("duplicate_table", "42P07"),
    ("error_in_assignment", "22005"),
    ("escape_character_conflict", "2200B"),
    ("event_trigger_protocol_violated", "39P03"),
    ("exclusion_violation", "23P01"),
    ("external_routine_exception", "38000"),
    ("external_routine_invocation_exception", "39000"),
    ("fdw_column_name_not_found", "HV005"),
    ("fdw_dynamic_parameter_value_needed", "HV002"),
    ("fdw_error", "HV000"),
    ("fdw_function_sequence_error", "HV010"),
    ("fdw_inconsistent_descriptor_information", "HV021"),
    ("fdw_invalid_attribute_value", "HV024"),
    ("fdw_invalid_column_name", "HV007"),
    ("fdw_invalid_column_number", "HV008"),
    ("fdw_invalid_data_type", "HV004"),
    ("fdw_invalid_data_type_descriptors", "HV006"),
    ("fdw_invalid_descriptor_field_identifier", "HV091"),
    ("fdw_invalid_handle", "HV00B"),
    ("fdw_invalid_option_index", "HV00C"),
    ("fdw_invalid_option_name", "HV00D"),
    ("fdw_invalid_string_format", "HV00A"),
    ("fdw_invalid_string_length_or_buffer_length", "HV090"),
    ("fdw_invalid_use_of_null_pointer", "HV009"),
    ("fdw_no_schemas", "HV00P"),
    ("fdw_option_name_not_found", "HV00J"),
    ("fdw_out_of_memory", "HV001"),
    ("fdw_reply_handle", "HV00K"),
    ("fdw_schema_not_found", "HV00Q"),
    ("fdw_table_not_found", "HV00R"),
    ("fdw_too_many_handles", "HV014"),
    ("fdw_unable_to_create_execution", "HV00L"),
    ("fdw_unable_to_create_reply", "HV00M"),
    ("fdw_unable_to_establish_connection", "HV00N"),
    ("feature_not_supported", "0A000"),
    ("file_name_too_long", "58P03"),
    ("floating_point_exception", "22P01"),
    ("foreign_key_violation", "23503"),
    ("function_executed_no_return_statement", "2F005"),
    ("generated_always", "428C9"),
    ("grouping_error", "42803"),
    ("held_cursor_requires_same_isolation_level", "25008"),
    ("idle_in_transaction_session_timeout", "25P03"),
    ("idle_session_timeout", "57P05"),
    ("in_failed_sql_transaction", "25P02"),
    ("inappropriate_access_mode_for_branch_transaction", "25003"),
    (
        "inappropriate_isolation_level_for_branch_transaction",
        "25004",
    ),
    ("indeterminate_collation", "42P22"),
    ("indeterminate_datatype", "42P18"),
    ("index_corrupted", "XX002"),
    ("indicator_overflow", "22022"),
    ("insufficient_privilege", "42501"),
    ("insufficient_resources", "53000"),
    ("integrity_constraint_violation", "23000"),
    ("internal_error", "XX000"),
    ("interval_field_overflow", "22015"),
    ("invalid_argument_for_logarithm", "2201E"),
    ("invalid_argument_for_nth_value_function", "22016"),
    ("invalid_argument_for_ntile_function", "22014"),
    ("invalid_argument_for_power_function", "2201F"),
    ("invalid_argument_for_sql_json_datetime_function", "22031"),
    ("invalid_argument_for_width_bucket_function", "2201G"),
    ("invalid_argument_for_xquery", "10608"),
    ("invalid_authorization_specification", "28000"),
    ("invalid_binary_representation", "22P03"),
    ("invalid_catalog_name", "3D000"),
    ("invalid_character_value_for_cast", "22018"),
    ("invalid_column_definition", "42611"),
    ("invalid_column_reference", "42P10"),
    ("invalid_cursor_definition", "42P11"),
    ("invalid_cursor_name", "34000"),
    ("invalid_cursor_state", "24000"),
    ("invalid_database_definition", "42P12"),
    ("invalid_datetime_format", "22007"),
    ("invalid_escape_character", "22019"),
    ("invalid_escape_octet", "2200D"),
    ("invalid_escape_sequence", "22025"),
    ("invalid_foreign_key", "42830"),
    ("invalid_function_definition", "42P13"),
    ("invalid_grant_operation", "0LP01"),
    ("invalid_grantor", "0L000"),
    ("invalid_indicator_parameter_value", "22010"),
    ("invalid_json_text", "22032"),
    ("invalid_locator_specification", "0F001"),
    ("invalid_name", "42602"),
    ("invalid_object_definition", "42P17"),
    ("invalid_parameter_value", "22023"),
    ("invalid_password", "28P01"),
    ("invalid_preceding_or_following_size", "22013"),
    ("invalid_prepared_statement_definition", "42P14"),
    ("invalid_recursion", "42P19"),
    ("invalid_regular_expression", "2201B"),
    ("invalid_role_specification", "0P000"),
    ("invalid_row_count_in_limit_clause", "2201W"),
    ("invalid_row_count_in_result_offset_clause", "2201X"),
    ("invalid_savepoint_specification", "3B001"),
    ("invalid_schema_definition", "42P15"),
    ("invalid_schema_name", "3F000"),
    ("invalid_sql_json_subscript", "22033"),
    ("invalid_sql_statement_name", "26000"),
    ("invalid_sqlstate_returned", "39001"),
    ("invalid_table_definition", "42P16"),
    ("invalid_tablesample_argument", "2202H"),
    ("invalid_tablesample_repeat", "2202G"),
    ("invalid_text_representation", "22P02"),
    ("invalid_time_zone_displacement_value", "22009"),
    ("invalid_transaction_initiation", "0B000"),
    ("invalid_transaction_state", "25000"),
    ("invalid_transaction_termination", "2D000"),
    ("invalid_use_of_escape_character", "2200C"),
    ("invalid_xml_comment", "2200S"),
    ("invalid_xml_content", "2200N"),
    ("invalid_xml_document", "2200M"),
    ("invalid_xml_processing_instruction", "2200T"),
    ("io_error", "58030"),
    ("locator_exception", "0F000"),
    ("lock_file_exists", "F0001"),
    ("lock_not_available", "55P03"),
    ("modifying_sql_data_not_permitted", "2F002"),
    ("modifying_sql_data_not_permitted", "38002"),
    ("more_than_one_sql_json_item", "22034"),
    ("most_specific_type_mismatch", "2200G"),
    ("name_too_long", "42622"),
    ("no_active_sql_transaction", "25P01"),
    ("no_active_sql_transaction_for_branch_transaction", "25005"),
    ("no_data_found", "P0002"),
    ("no_sql_json_item", "22035"),
    ("non_numeric_sql_json_item", "22036"),
    ("non_unique_keys_in_a_json_object", "22037"),
    ("nonstandard_use_of_escape_character", "22P06"),
    ("not_an_xml_document", "2200L"),
    ("not_null_violation", "23502"),
    ("null_value_no_indicator_parameter", "22002"),
    ("null_value_not_allowed", "22004"),
    ("null_value_not_allowed", "39004"),
    ("numeric_value_out_of_range", "22003"),
    ("object_in_use", "55006"),
    ("object_not_in_prerequisite_state", "55000"),
    ("operator_intervention", "57000"),
    ("out_of_memory", "53200"),
    ("plpgsql_error", "P0000"),
    ("program_limit_exceeded", "54000"),
    ("prohibited_sql_statement_attempted", "2F003"),
    ("prohibited_sql_statement_attempted", "38003"),
    ("protocol_violation", "08P01"),
    ("query_canceled", "57014"),
    ("raise_exception", "P0001"),
    ("read_only_sql_transaction", "25006"),
    ("reading_sql_data_not_permitted", "2F004"),
    ("reading_sql_data_not_permitted", "38004"),
    ("reserved_name", "42939"),
    ("restrict_violation", "23001"),
    ("savepoint_exception", "3B000"),
    ("schema_and_data_statement_mixing_not_supported", "25007"),
    ("sequence_generator_limit_exceeded", "2200H"),
    ("serialization_failure", "40001"),
    ("singleton_sql_json_item_required", "22038"),
    ("sql_json_array_not_found", "22039"),
    ("sql_json_item_cannot_be_cast_to_target_type", "2203G"),
    ("sql_json_member_not_found", "2203A"),
    ("sql_json_number_not_found", "2203B"),
    ("sql_json_object_not_found", "2203C"),
    ("sql_json_scalar_required", "2203F"),
    ("sql_routine_exception", "2F000"),
    ("sql_statement_not_yet_complete", "03000"),
    ("sqlclient_unable_to_establish_sqlconnection", "08001"),
    ("sqlserver_rejected_establishment_of_sqlconnection", "08004"),
    ("srf_protocol_violated", "39P02"),
    (
        "stacked_diagnostics_accessed_without_active_handler",
        "0Z002",
    ),
    ("statement_completion_unknown", "40003"),
    ("statement_too_complex", "54001"),
    ("string_data_length_mismatch", "22026"),
    ("string_data_right_truncation", "22001"),
    ("substring_error", "22011"),
    ("syntax_error", "42601"),
    ("syntax_error_or_access_rule_violation", "42000"),
    ("system_error", "58000"),
    ("too_many_arguments", "54023"),
    ("too_many_columns", "54011"),
    ("too_many_connections", "53300"),
    ("too_many_json_array_elements", "2203D"),
    ("too_many_json_object_members", "2203E"),
    ("too_many_rows", "P0003"),
    ("transaction_integrity_constraint_violation", "40002"),
    ("transaction_resolution_unknown", "08007"),
    ("transaction_rollback", "40000"),
    ("transaction_timeout", "25P04"),
    ("trigger_protocol_violated", "39P01"),
    ("triggered_action_exception", "09000"),
    ("triggered_data_change_violation", "27000"),
    ("trim_error", "22027"),
    ("undefined_column", "42703"),
    ("undefined_file", "58P01"),
    ("undefined_function", "42883"),
    ("undefined_object", "42704"),
    ("undefined_parameter", "42P02"),
    ("undefined_table", "42P01"),
    ("unique_violation", "23505"),
    ("unsafe_new_enum_value_usage", "55P04"),
    ("unterminated_c_string", "22024"),
    ("untranslatable_character", "22P05"),
    ("windowing_error", "42P20"),
    ("with_check_option_violation", "44000"),
    ("wrong_object_type", "42809"),
    ("zero_length_character_string", "2200F"),
];

/// v1.01: resolve one WHEN condition name to its SQLSTATE list (PG19
/// `plpgsql_parse_err_condition`, `pl_comp.c`). Names are folded to
/// lowercase like PG's unquoted identifiers; `others` yields the OTHERS
/// sentinel; one name may map to several codes (duplicate labels in
/// `errcodes.txt`, e.g. `null_value_not_allowed`). Unknown names are
/// 42704, mirroring PG's ERRCODE_UNDEFINED_OBJECT `unrecognized
/// exception condition`.
pub(crate) fn plpgsql_condition_named(name: &str) -> Result<Vec<String>, SqlError> {
    let folded = name.to_ascii_lowercase();
    if folded == "others" {
        return Ok(vec![PLPGSQL_OTHERS_SENTINEL.to_string()]);
    }
    let lo = PLPGSQL_CONDITIONS.partition_point(|e| e.0 < folded.as_str());
    let mut out = Vec::new();
    for (n, code) in &PLPGSQL_CONDITIONS[lo..] {
        if *n != folded.as_str() {
            break;
        }
        out.push((*code).to_string());
    }
    if out.is_empty() {
        return Err(SqlError {
            message: format!("unrecognized exception condition \"{}\"", name),
            code: "42704",
        });
    }
    Ok(out)
}

/// v1.01: does a trapped SQLSTATE match a raised error code? (PG19
/// `exception_matches_conditions`, `pl_exec.c`: exact match; category
/// match when the condition is a `...000` class code, i.e. PG's
/// `ERRCODE_IS_CATEGORY` / `ERRCODE_TO_CATEGORY`; OTHERS matches
/// everything except 57014 query_canceled and P0004 assert_failure.)
pub fn plpgsql_condition_matches(condition: &str, code: &str) -> bool {
    if condition == PLPGSQL_OTHERS_SENTINEL {
        return code != "57014" && code != "P0004";
    }
    if condition == code {
        return true;
    }
    let cb = condition.as_bytes();
    let eb = code.as_bytes();
    cb.len() == 5 && eb.len() == 5 && cb[2..] == *b"000" && cb[..2] == eb[..2]
}

/// Strip an ASCII keyword (case-insensitive) from the front of `s`,
/// requiring a word boundary after it. Returns the remainder.
pub(crate) fn plpgsql_strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.get(..kw.len())?;
    if !rest.eq_ignore_ascii_case(kw) {
        return None;
    }
    let after = s.get(kw.len()..)?;
    // Word boundary: next char must not be ident-continue. The keyword
    // itself may be followed by end-of-string.
    if let Some(c) = after.chars().next() {
        if c.is_alphanumeric() || c == '_' {
            return None;
        }
    }
    Some(after)
}

/// Does `s` start with `kw` (case-insensitive, word boundary)?
pub(crate) fn plpgsql_starts_with_kw(s: &str, kw: &str) -> bool {
    plpgsql_strip_kw(s.trim_start(), kw).is_some()
}

/// Strip a trailing END keyword (word boundary before it) from `s`.
/// Returns the remainder, or None if `s` does not end with END.
pub(crate) fn plpgsql_strip_end(s: &str) -> Option<&str> {
    let t = s.trim_end();
    if t.len() < 3 || !t[t.len() - 3..].eq_ignore_ascii_case("end") {
        return None;
    }
    let before = &t[..t.len() - 3];
    if let Some(c) = before.chars().last() {
        if c.is_alphanumeric() || c == '_' {
            return None;
        }
    }
    Some(before)
}

/// v1.01: split a plpgsql body on top-level `;`, respecting
/// single-quoted strings (`''` escapes), double-quoted identifiers
/// (`""` escapes), `--` / `/* */` comments, and `$tag$...$tag$`
/// dollar-quoted strings. `$1`-style parameters are NOT dollar quotes
/// (the char after `$` must not start a digit-led tag).
pub(crate) fn split_plpgsql_chunks(body: &str) -> Vec<String> {
    /// Length of the dollar-quote opening delimiter at `chars[i]`
    /// (`$$` or `$tag$`), or None if this `$` opens no dollar quote.
    fn dollar_open_len(chars: &[char], i: usize) -> Option<usize> {
        if chars[i] != '$' {
            return None;
        }
        let mut j = i + 1;
        if j < chars.len() && chars[j] == '$' {
            return Some(2);
        }
        let tag_start = j;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        if j == tag_start || chars[tag_start].is_ascii_digit() {
            return None;
        }
        if j < chars.len() && chars[j] == '$' {
            Some(j - i + 1)
        } else {
            None
        }
    }
    let chars: Vec<char> = body.chars().collect();
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                let q = c;
                cur.push(q);
                i += 1;
                while i < chars.len() {
                    let d = chars[i];
                    cur.push(d);
                    i += 1;
                    if d == q {
                        if i < chars.len() && chars[i] == q {
                            cur.push(q);
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if i + 1 < chars.len() && chars[i + 1] == '-' => {
                while i < chars.len() && chars[i] != '\n' {
                    cur.push(chars[i]);
                    i += 1;
                }
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                cur.push('/');
                cur.push('*');
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    cur.push(chars[i]);
                    i += 1;
                }
                if i + 1 < chars.len() {
                    cur.push('*');
                    cur.push('/');
                    i += 2;
                }
            }
            '$' => match dollar_open_len(&chars, i) {
                Some(len) => {
                    // Copy through the matching close delimiter; an
                    // unterminated quote swallows the rest (the
                    // statement parse will fail on it later).
                    let mut j = i + len;
                    let mut end = chars.len();
                    while j + len <= chars.len() {
                        if chars[j..j + len] == chars[i..i + len] {
                            end = j + len;
                            break;
                        }
                        j += 1;
                    }
                    cur.extend(chars[i..end].iter());
                    i = end;
                }
                None => {
                    cur.push(c);
                    i += 1;
                }
            },
            ';' => {
                chunks.push(std::mem::take(&mut cur));
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    chunks.push(cur);
    chunks
}

/// v1.01: parse one plpgsql statement — `RETURN <expr>` or a supported
/// utility statement. Anything else is an honest 0A000 (outside the
/// bounded subset); a malformed RETURN expression is 42601.
/// v1.02: split `s` on top-level commas: commas inside quotes
/// (single-quoted with `''` escapes, double-quoted), comments are not
/// produced here (the body chunker strips them), or nested
/// `(...)` / `[...]` do not split. Used for the RAISE argument list.
pub(crate) fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: usize = 0;
    let mut start: usize = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' | '"' => {
                let q = c;
                while let Some((_, d)) = chars.next() {
                    if d == q {
                        if chars.peek().is_some_and(|(_, e)| *e == q) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// v1.02: parse `RAISE <level> '<format>' [, <expr> ...]` (the RAISE
/// keyword already stripped). PG19 pl_gram.y `stmt_raise`.
///
/// Only NOTICE and EXCEPTION are supported in the bounded grammar;
/// any other level (DEBUG/LOG/INFO/WARNING) is an honest 0A000,
/// mirroring the v1.00 trigger-body rule. Each argument parses as a
/// scalar expression via the same `SELECT <expr>` trick the RETURN
/// path uses, so the named-argument `rewrite_func_arg_refs` machinery
/// in exec.rs applies to RAISE args unchanged.
pub(crate) fn parse_plpgsql_raise(rest: &str) -> Result<PlpgsqlStmt, SqlError> {
    let syntax = |msg: String| SqlError {
        message: msg,
        code: "42601",
    };
    let rest = rest.trim_start();
    // Level word (word boundary, like plpgsql_strip_kw).
    let mut lvl_len = 0;
    for (i, c) in rest.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            lvl_len = i + c.len_utf8();
        } else {
            break;
        }
    }
    let (lvl_word, after_lvl) = rest.split_at(lvl_len);
    let level = match lvl_word.to_ascii_lowercase().as_str() {
        "notice" => RaiseLevel::Notice,
        "exception" => RaiseLevel::Exception,
        _ => {
            return Err(SqlError {
                message: format!(
                    "RAISE level '{}' is not supported in plpgsql function bodies",
                    lvl_word
                ),
                code: "0A000",
            });
        }
    };
    // Format string: a single-quoted literal with '' escapes.
    let after_lvl = after_lvl.trim_start();
    let lit = after_lvl
        .strip_prefix('\'')
        .ok_or_else(|| syntax("RAISE requires a format string literal".to_string()))?;
    let mut format = String::new();
    let mut chars = lit.char_indices().peekable();
    let mut end_byte: Option<usize> = None;
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if chars.peek().is_some_and(|(_, d)| *d == '\'') {
                format.push('\'');
                chars.next();
            } else {
                end_byte = Some(i);
                break;
            }
        } else {
            format.push(c);
        }
    }
    let end_byte =
        end_byte.ok_or_else(|| syntax("unterminated string literal in RAISE".to_string()))?;
    let after_fmt = lit[end_byte + 1..].trim_start();
    // Optional `, <expr> ...` argument list.
    let mut args = Vec::new();
    if !after_fmt.is_empty() {
        let list = after_fmt
            .strip_prefix(',')
            .ok_or_else(|| syntax("expected ',' after RAISE format string".to_string()))?;
        for part in split_top_level_commas(list) {
            let part = part.trim();
            if part.is_empty() {
                return Err(syntax(
                    "empty expression in RAISE argument list".to_string(),
                ));
            }
            let stmt = parse_statement(&format!("SELECT {}", part)).map_err(|e| SqlError {
                message: format!("syntax error in RAISE argument: {}", e.message),
                code: "42601",
            })?;
            let Stmt::Select(sel) = stmt else {
                return Err(SqlError {
                    message: "internal error: RAISE argument did not parse as SELECT".to_string(),
                    code: "XX000",
                });
            };
            let mut items = sel.items.into_iter();
            match (items.next(), items.next()) {
                (Some(SelectItem::Expr { expr, .. }), None) => args.push(expr),
                _ => {
                    return Err(syntax(format!(
                        "RAISE argument is not a scalar expression: {}",
                        part.chars().take(40).collect::<String>()
                    )));
                }
            }
        }
    }
    Ok(PlpgsqlStmt::Raise {
        level,
        format,
        args,
    })
}

// ============================================================================
// v1.03: plpgsql DECLARE / := / FOR..LOOP / RETURN NEXT
// ============================================================================

/// Skip a `$tag$...$tag$` (or `$$...$$`) dollar-quoted string starting at
/// `chars[i] == '$'`. Returns the index just past the closing delimiter,
/// or `None` if this `$` opens no dollar quote (`$1`-style parameters
/// are not dollar quotes: the char after `$` must not start a digit-led
/// tag).
pub(crate) fn plpgsql_dollar_skip(chars: &[char], i: usize) -> Option<usize> {
    if chars[i] != '$' {
        return None;
    }
    let mut j = i + 1;
    if j < chars.len() && chars[j] == '$' {
        j += 1;
        while j + 1 < chars.len() && !(chars[j] == '$' && chars[j + 1] == '$') {
            j += 1;
        }
        return Some((j + 2).min(chars.len()));
    }
    let tag_start = j;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if j == tag_start || chars[tag_start].is_ascii_digit() {
        return None;
    }
    if j < chars.len() && chars[j] == '$' {
        let delim: Vec<char> = chars[i..=j].to_vec();
        let dl = delim.len();
        let mut k = j + 1;
        while k + dl <= chars.len() && chars[k..k + dl] != delim[..] {
            k += 1;
        }
        return Some((k + dl).min(chars.len()));
    }
    None
}

/// Byte positions of every occurrence of `tok` in `s`, skipping
/// single/double-quoted strings, `--` / `/* */` comments, and
/// `$tag$...$tag$` dollar quotes. When `word` is true the match also
/// requires identifier word boundaries (for keywords); when false any
/// occurrence counts (for the `:=` operator).
pub(crate) fn plpgsql_find_token(s: &str, tok: &str, word: bool) -> Vec<usize> {
    let chars: Vec<char> = s.chars().collect();
    let byte_off: Vec<usize> = s.char_indices().map(|(b, _)| b).collect();
    let tok_chars: Vec<char> = tok.chars().collect();
    let tl = tok_chars.len();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            i += 1;
            while i < chars.len() {
                if chars[i] == c {
                    if i + 1 < chars.len() && chars[i + 1] == c {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
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
            i = (i + 2).min(chars.len());
            continue;
        }
        if c == '$' {
            if let Some(next) = plpgsql_dollar_skip(&chars, i) {
                i = next;
                continue;
            }
            i += 1;
            continue;
        }
        if i + tl <= chars.len() && chars[i..i + tl] == tok_chars[..] {
            let mut ok = true;
            if word {
                // `tok` is ASCII here, so byte slicing is safe.
                let b = byte_off[i];
                if !s[b..b + tok.len()].eq_ignore_ascii_case(tok) {
                    ok = false;
                } else {
                    let before_ok =
                        i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
                    let after_ok = i + tl == chars.len()
                        || !(chars[i + tl].is_alphanumeric() || chars[i + tl] == '_');
                    ok = before_ok && after_ok;
                }
            }
            if ok {
                pos.push(byte_off[i]);
            }
        }
        i += 1;
    }
    pos
}

/// Byte positions of keyword `kw` (case-insensitive, word boundaries)
/// outside strings/comments/dollar quotes.
pub(crate) fn plpgsql_kw_positions(s: &str, kw: &str) -> Vec<usize> {
    plpgsql_find_token(s, kw, true)
}

/// Byte index of the first `:=` outside strings/comments/dollar quotes.
pub(crate) fn plpgsql_find_assign(s: &str) -> Option<usize> {
    plpgsql_find_token(s, ":=", false).into_iter().next()
}

/// Does `s` end with `END LOOP` (word boundaries, case-insensitive)?
/// A trailing quote always defeats the match, so a string literal
/// ending in the words `end loop` can never falsely close a FOR loop.
pub(crate) fn plpgsql_ends_with_end_loop(s: &str) -> bool {
    let t = s.trim_end();
    if t.len() < 8 || !t[t.len() - 8..].eq_ignore_ascii_case("end loop") {
        return false;
    }
    match t[..t.len() - 8].chars().last() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_'),
    }
}

/// Strip a trailing `END LOOP` (word boundaries, case-insensitive) from
/// `s`. Returns the remainder, or `None`.
pub(crate) fn plpgsql_strip_end_loop(s: &str) -> Option<&str> {
    if !plpgsql_ends_with_end_loop(s) {
        return None;
    }
    let t = s.trim_end();
    Some(t[..t.len() - 8].trim_end())
}

/// v1.03: parse `RETURN <expr>` (the `RETURN` keyword already stripped).
pub(crate) fn parse_plpgsql_return(rest: &str) -> Result<PlpgsqlStmt, SqlError> {
    let expr = rest.trim();
    if expr.is_empty() {
        return Err(SqlError {
            message: "RETURN requires an expression".to_string(),
            code: "42601",
        });
    }
    let stmt = parse_statement(&format!("SELECT {}", expr)).map_err(|e| SqlError {
        message: format!("syntax error in RETURN expression: {}", e.message),
        code: "42601",
    })?;
    if !matches!(stmt, Stmt::Select(_)) {
        return Err(SqlError {
            message: "internal error: RETURN did not parse as SELECT".to_string(),
            code: "XX000",
        });
    }
    Ok(PlpgsqlStmt::Return(stmt))
}

/// v1.03: parse `RETURN NEXT <expr>` (both keywords already stripped).
pub(crate) fn parse_plpgsql_return_next(rest: &str) -> Result<PlpgsqlStmt, SqlError> {
    let expr = rest.trim();
    if expr.is_empty() {
        return Err(SqlError {
            message: "RETURN NEXT requires an expression".to_string(),
            code: "42601",
        });
    }
    let stmt = parse_statement(&format!("SELECT {}", expr)).map_err(|e| SqlError {
        message: format!("syntax error in RETURN NEXT expression: {}", e.message),
        code: "42601",
    })?;
    if !matches!(stmt, Stmt::Select(_)) {
        return Err(SqlError {
            message: "internal error: RETURN NEXT did not parse as SELECT".to_string(),
            code: "XX000",
        });
    }
    Ok(PlpgsqlStmt::ReturnNext(stmt))
}

/// v1.03: parse `var := <expr>` (the statement is known to contain a
/// depth-0 `:=`). The variable name is validated against the DECLAREd
/// names at CREATE time, not here.
pub(crate) fn parse_plpgsql_assign(text: &str) -> Result<PlpgsqlStmt, SqlError> {
    let syntax = |msg: String| SqlError {
        message: msg,
        code: "42601",
    };
    let t = text.trim();
    let op =
        plpgsql_find_assign(t).ok_or_else(|| syntax("bad assignment statement".to_string()))?;
    let (lhs, rhs) = t.split_at(op);
    let (var, var_rest) =
        split_ident(lhs).ok_or_else(|| syntax(format!("bad assignment target: {}", lhs.trim())))?;
    if !var_rest.trim().is_empty() {
        return Err(syntax(format!("bad assignment target: {}", lhs.trim())));
    }
    let expr = rhs[2..].trim();
    if expr.is_empty() {
        return Err(syntax(":= requires an expression".to_string()));
    }
    let stmt = parse_statement(&format!("SELECT {}", expr)).map_err(|e| SqlError {
        message: format!("syntax error in assignment expression: {}", e.message),
        code: "42601",
    })?;
    if !matches!(stmt, Stmt::Select(_)) {
        return Err(SqlError {
            message: "internal error: assignment did not parse as SELECT".to_string(),
            code: "XX000",
        });
    }
    Ok(PlpgsqlStmt::Assign { var, select: stmt })
}

/// v1.03: parse one `name type` declaration (trailing `;` already split
/// off). Only plain scalar `name type` is accepted: no initializers,
/// no CONSTANT, no constraints (bounded subset, 0A000).
pub(crate) fn parse_plpgsql_decl(text: &str) -> Result<PlpgsqlDecl, SqlError> {
    let syntax = |msg: String| SqlError {
        message: msg,
        code: "42601",
    };
    let unsupported = |msg: String| SqlError {
        message: msg,
        code: "0A000",
    };
    let t = text.trim();
    if plpgsql_starts_with_kw(t, "constant") {
        return Err(unsupported(
            "CONSTANT variable declarations are not supported".to_string(),
        ));
    }
    let (name, rest) =
        split_ident(t).ok_or_else(|| syntax(format!("bad variable declaration: {}", t)))?;
    let type_name = rest.trim();
    if type_name.is_empty() {
        return Err(syntax(format!("declaration of \"{}\" needs a type", name)));
    }
    // Anything beyond a bare type name (constraints, defaults) is out
    // of the bounded subset.
    for kw in ["not", "null", "default", "collate", "check", "references"] {
        if !plpgsql_kw_positions(type_name, kw).is_empty() {
            return Err(unsupported(format!(
                "only plain \"name type\" declarations are supported, got: {}",
                t
            )));
        }
    }
    Ok(PlpgsqlDecl {
        name,
        type_name: type_name.to_string(),
    })
}

/// v1.03: parse a `DECLARE` section (text between `DECLARE` and the
/// `BEGIN` that opens the body block).
pub(crate) fn parse_plpgsql_decls(decl_text: &str) -> Result<Vec<PlpgsqlDecl>, SqlError> {
    let syntax = |msg: String| SqlError {
        message: msg,
        code: "42601",
    };
    let unsupported = |msg: String| SqlError {
        message: msg,
        code: "0A000",
    };
    let mut decls = Vec::new();
    for chunk in split_plpgsql_chunks(decl_text) {
        let t = chunk.trim();
        if t.is_empty() {
            continue;
        }
        if plpgsql_find_assign(t).is_some() {
            return Err(unsupported(format!(
                "variable initializers (:=) are not supported in DECLARE: {}",
                t.chars().take(40).collect::<String>()
            )));
        }
        let decl = parse_plpgsql_decl(t)?;
        if decls.iter().any(|d: &PlpgsqlDecl| d.name == decl.name) {
            return Err(syntax(format!(
                "duplicate variable declaration: \"{}\"",
                decl.name
            )));
        }
        decls.push(decl);
    }
    if decls.is_empty() {
        return Err(syntax(
            "DECLARE section without variable declarations".to_string(),
        ));
    }
    Ok(decls)
}

/// v1.03: parse one statement of a FOR-loop body. Only `:=` assignment,
/// `RETURN NEXT`, and bare `RETURN` are allowed (bounded subset).
pub(crate) fn parse_plpgsql_for_body_stmt(text: &str) -> Result<PlpgsqlStmt, SqlError> {
    let t = text.trim();
    if let Some(rest) = plpgsql_strip_kw(t, "return") {
        if let Some(after) = plpgsql_strip_kw(rest.trim(), "next") {
            return parse_plpgsql_return_next(after);
        }
        return parse_plpgsql_return(rest);
    }
    if plpgsql_find_assign(t).is_some() {
        return parse_plpgsql_assign(t);
    }
    Err(SqlError {
        message: format!(
            "unsupported statement in FOR loop body (only :=, RETURN, and RETURN NEXT are supported): {}",
            t.chars().take(60).collect::<String>()
        ),
        code: "0A000",
    })
}

/// v1.03: parse `FOR var IN <query> LOOP <stmts> END LOOP` (`text`
/// includes the trailing `END LOOP`). The query must be a `SELECT` or
/// `EXPLAIN`; the loop variable must be DECLAREd (checked at CREATE).
/// Nested FOR loops are not supported (0A000).
pub(crate) fn parse_plpgsql_for(text: &str) -> Result<PlpgsqlStmt, SqlError> {
    let syntax = |msg: String| SqlError {
        message: msg,
        code: "42601",
    };
    let t = text.trim();
    let rest = plpgsql_strip_kw(t, "for").ok_or_else(|| syntax("expected FOR".to_string()))?;
    let (var, rest) =
        split_ident(rest).ok_or_else(|| syntax("FOR requires a loop variable".to_string()))?;
    let rest = plpgsql_strip_kw(rest.trim_start(), "in")
        .ok_or_else(|| syntax("expected IN after FOR loop variable".to_string()))?;
    // Find LOOP: the query is the first depth-0 `LOOP`-terminated prefix
    // that parses as a statement (a column named `loop` must not end the
    // query early).
    let mut found: Option<(&str, &str)> = None;
    let mut first_err: Option<SqlError> = None;
    for lp in plpgsql_kw_positions(rest, "loop") {
        let q = rest[..lp].trim();
        if q.is_empty() {
            continue;
        }
        match parse_statement(q) {
            Ok(_) => {
                found = Some((q, rest[lp + 4..].trim()));
                break;
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    let (query_text, body_after) = found.ok_or_else(|| {
        first_err.unwrap_or_else(|| syntax("FOR requires IN <query> LOOP".to_string()))
    })?;
    let inner = plpgsql_strip_end_loop(body_after)
        .ok_or_else(|| syntax("FOR loop without END LOOP".to_string()))?;
    let query = parse_statement(query_text).map_err(|e| SqlError {
        message: format!("syntax error in FOR loop query: {}", e.message),
        code: "42601",
    })?;
    match query {
        Stmt::Select(_) | Stmt::Explain { .. } => {}
        _ => {
            return Err(syntax(
                "FOR loop query must be SELECT or EXPLAIN".to_string(),
            ));
        }
    }
    let mut body = Vec::new();
    for chunk in split_plpgsql_chunks(inner) {
        let c = chunk.trim();
        if c.is_empty() {
            continue;
        }
        let stmt = parse_plpgsql_for_body_stmt(c)?;
        plpgsql_push_stmt(&mut body, stmt, "FOR loop body")?;
    }
    Ok(PlpgsqlStmt::ForQuery { var, query, body })
}

/// v1.03: reject RETURN/RETURN NEXT placement against the function's
/// declared shape: `RETURN <expr>` is illegal in a `RETURNS SETOF`
/// function (use `RETURN NEXT`), and `RETURN NEXT` is illegal in a
/// scalar function (PG19 pl_comp.c `check_sql_stmt` equivalents).
pub(crate) fn plpgsql_validate_returns(
    stmts: &[PlpgsqlStmt],
    handlers: &[PlpgsqlHandler],
    returns_set: bool,
) -> Result<(), SqlError> {
    fn walk(stmts: &[PlpgsqlStmt], returns_set: bool) -> Result<(), SqlError> {
        for s in stmts {
            match s {
                PlpgsqlStmt::Return(_) if returns_set => {
                    return Err(SqlError {
                        message: "RETURN with a value cannot be used in a function returning set; use RETURN NEXT".to_string(),
                        code: "42601",
                    });
                }
                PlpgsqlStmt::ReturnNext(_) if !returns_set => {
                    return Err(SqlError {
                        message: "RETURN NEXT cannot be used in a non-SETOF function".to_string(),
                        code: "42601",
                    });
                }
                PlpgsqlStmt::ForQuery { body, .. } => walk(body, returns_set)?,
                _ => {}
            }
        }
        Ok(())
    }
    walk(stmts, returns_set)?;
    for h in handlers {
        walk(&h.stmts, returns_set)?;
    }
    Ok(())
}

/// v1.03: every `FOR` loop variable and `:=` assignment target must be
/// DECLAREd (PG requires a declared variable for both; an undeclared
/// name is 42601 at CREATE, like PG's plpgsql validator).
pub(crate) fn plpgsql_validate_vars(
    decls: &[PlpgsqlDecl],
    stmts: &[PlpgsqlStmt],
    handlers: &[PlpgsqlHandler],
) -> Result<(), SqlError> {
    fn walk(stmts: &[PlpgsqlStmt], decls: &[PlpgsqlDecl]) -> Result<(), SqlError> {
        for s in stmts {
            match s {
                PlpgsqlStmt::ForQuery { var, body, .. } => {
                    if !decls.iter().any(|d| d.name == *var) {
                        return Err(SqlError {
                            message: format!("FOR loop variable \"{}\" is not declared", var),
                            code: "42601",
                        });
                    }
                    walk(body, decls)?;
                }
                PlpgsqlStmt::Assign { var, .. } => {
                    if !decls.iter().any(|d| d.name == *var) {
                        return Err(SqlError {
                            message: format!("assignment target \"{}\" is not declared", var),
                            code: "42601",
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    walk(stmts, decls)?;
    for h in handlers {
        walk(&h.stmts, decls)?;
    }
    Ok(())
}

pub(crate) fn parse_plpgsql_stmt(text: &str) -> Result<PlpgsqlStmt, SqlError> {
    fn unsupported(text: &str) -> SqlError {
        SqlError {
            message: format!(
                "unsupported statement in plpgsql body (only RETURN, RETURN NEXT, :=, FOR..LOOP, RAISE, and ANALYZE are supported): {}",
                text.chars().take(60).collect::<String>()
            ),
            code: "0A000",
        }
    }
    let t = text.trim();
    if let Some(rest) = plpgsql_strip_kw(t, "return") {
        // v1.03: `RETURN NEXT <expr>` (SETOF functions only; placement is
        // validated against `returns_set` in `parse_plpgsql_body`).
        if let Some(after) = plpgsql_strip_kw(rest.trim(), "next") {
            return parse_plpgsql_return_next(after);
        }
        return parse_plpgsql_return(rest);
    }
    // v1.02: `RAISE NOTICE|EXCEPTION '<format>' [, <expr> ...]`.
    if let Some(rest) = plpgsql_strip_kw(t, "raise") {
        return parse_plpgsql_raise(rest);
    }
    // v1.03: `var := <expr>` scalar assignment.
    if plpgsql_find_assign(t).is_some() {
        return parse_plpgsql_assign(t);
    }
    let stmt = parse_statement(t).map_err(|_| unsupported(t))?;
    match stmt {
        Stmt::Analyze { .. } => Ok(PlpgsqlStmt::Utility(stmt)),
        _ => Err(unsupported(t)),
    }
}

/// v1.01: parse one WHEN condition (`name` or `SQLSTATE 'code'`,
/// PG19 pl_gram.y `proc_condition`). Returns the condition's SQLSTATEs
/// and the unconsumed remainder.
pub(crate) fn parse_plpgsql_condition(p: &str) -> Result<(Vec<String>, &str), SqlError> {
    let p = p.trim_start();
    if let Some(after) = plpgsql_strip_kw(p, "sqlstate") {
        let lit = after.trim_start();
        let rest = lit.strip_prefix('\'').ok_or_else(|| SqlError {
            message: "SQLSTATE condition requires a string literal".to_string(),
            code: "42601",
        })?;
        let end = rest.find('\'').ok_or_else(|| SqlError {
            message: "SQLSTATE condition requires a string literal".to_string(),
            code: "42601",
        })?;
        let code = &rest[..end];
        // PG validates: exactly 5 chars of 0-9A-Z (pl_gram.y).
        if code.len() != 5
            || !code
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
        {
            return Err(SqlError {
                message: "invalid SQLSTATE code".to_string(),
                code: "42601",
            });
        }
        return Ok((vec![code.to_string()], &rest[end + 1..]));
    }
    let mut id_len = 0;
    for (i, c) in p.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            id_len = i + c.len_utf8();
        } else {
            break;
        }
    }
    if id_len == 0 {
        return Err(SqlError {
            message: "syntax error in WHEN condition".to_string(),
            code: "42601",
        });
    }
    let (name, restp) = p.split_at(id_len);
    Ok((plpgsql_condition_named(name)?, restp))
}

/// v1.01: parse one `WHEN <conditions> THEN <statement>` clause (the
/// WHEN keyword already stripped; PG19 pl_gram.y `proc_exception`).
/// Conditions are `cond [OR cond ...]`; each condition is a name or
/// `SQLSTATE 'code'`.
pub(crate) fn parse_plpgsql_when(rest: &str) -> Result<PlpgsqlHandler, SqlError> {
    let mut sqlstates = Vec::new();
    let mut p = rest;
    loop {
        let (codes, restp) = parse_plpgsql_condition(p)?;
        sqlstates.extend(codes);
        p = restp.trim_start();
        if let Some(after) = plpgsql_strip_kw(p, "or") {
            p = after;
            continue;
        }
        break;
    }
    let after = plpgsql_strip_kw(p, "then").ok_or_else(|| SqlError {
        message: "expected THEN in WHEN clause".to_string(),
        code: "42601",
    })?;
    let stmt_text = after.trim();
    if stmt_text.is_empty() {
        return Err(SqlError {
            message: "WHEN ... THEN requires a statement".to_string(),
            code: "42601",
        });
    }
    Ok(PlpgsqlHandler {
        sqlstates,
        stmts: vec![parse_plpgsql_stmt(stmt_text)?],
    })
}

/// v1.01: append an implicit `RETURN NULL` unless the statement list
/// already ends with RETURN (PG19 `add_dummy_return`, `pl_comp.c`).
pub(crate) fn plpgsql_ensure_trailing_return(stmts: &mut Vec<PlpgsqlStmt>) -> Result<(), SqlError> {
    let needs = !matches!(stmts.last(), Some(PlpgsqlStmt::Return(_)));
    if needs {
        let null_sel = parse_statement("SELECT NULL").map_err(|e| SqlError {
            message: format!("internal error building implicit RETURN: {}", e.message),
            code: "XX000",
        })?;
        stmts.push(PlpgsqlStmt::Return(null_sel));
    }
    Ok(())
}

/// v1.01: push a statement onto a bounded statement list, enforcing
/// that RETURN ends the list: at most one RETURN per list and nothing
/// may follow it. (PG would treat a second RETURN as dead code; the
/// bounded subset keeps that case an honest 0A000.)
pub(crate) fn plpgsql_push_stmt(
    stmts: &mut Vec<PlpgsqlStmt>,
    stmt: PlpgsqlStmt,
    ctx: &str,
) -> Result<(), SqlError> {
    if stmts.iter().any(|s| matches!(s, PlpgsqlStmt::Return(_))) {
        return Err(SqlError {
            message: format!("statement after RETURN in plpgsql {ctx}"),
            code: "0A000",
        });
    }
    stmts.push(stmt);
    Ok(())
}

/// v1.01: parse a bounded PL/pgSQL body:
/// `[DECLARE <decls>] BEGIN <stmts> [EXCEPTION <handlers>] END`
/// (PG19 pl_gram.y `pl_block` / `decl_sect` / `exception_sect`; no labels).
///
/// Each statement is `RETURN <expr>`, `RETURN NEXT <expr>` (SETOF only),
/// `var := <expr>`, `FOR var IN <query> LOOP <stmts> END LOOP`, `RAISE`,
/// or a supported utility statement (currently only ANALYZE); each
/// handler is `WHEN <cond> [OR <cond> ...] THEN <statement>`, and
/// further `;`-separated statements after the THEN belong to the same
/// handler until the next WHEN or END.
///
/// `returns_set` selects the function's declared shape so RETURN /
/// RETURN NEXT placement can be validated the way PG's validator does
/// at CREATE time.
pub fn parse_plpgsql_body(body: &str, returns_set: bool) -> Result<PlpgsqlBody, SqlError> {
    let unsupported = |msg: String| SqlError {
        message: msg,
        code: "0A000",
    };
    let syntax = |msg: String| SqlError {
        message: msg,
        code: "42601",
    };
    // v1.03: optional DECLARE section before BEGIN.
    let mut decls: Vec<PlpgsqlDecl> = Vec::new();
    let mut code = body;
    if plpgsql_starts_with_kw(body.trim_start(), "declare") {
        let after = plpgsql_strip_kw(body.trim_start(), "declare").unwrap_or("");
        let bp = plpgsql_kw_positions(after, "begin")
            .into_iter()
            .next()
            .ok_or_else(|| syntax("DECLARE section without BEGIN".to_string()))?;
        decls = parse_plpgsql_decls(after[..bp].trim())?;
        code = after[bp..].trim_start();
    }
    let mut chunks = split_plpgsql_chunks(code);
    while chunks.last().is_some_and(|c| c.trim().is_empty()) {
        chunks.pop();
    }
    let n = chunks.len();
    if n == 0 {
        return Err(unsupported("empty plpgsql body".to_string()));
    }
    // Statement texts in order: first chunk (BEGIN stripped), middle
    // chunks, last chunk (END stripped).
    let mut texts: Vec<String> = Vec::with_capacity(n);
    if n == 1 {
        let rest = plpgsql_strip_kw(chunks[0].trim(), "begin").ok_or_else(|| {
            unsupported(format!(
                "plpgsql body must start with BEGIN, got: {}",
                chunks[0].trim().chars().take(40).collect::<String>()
            ))
        })?;
        let inner = plpgsql_strip_end(rest)
            .ok_or_else(|| syntax("plpgsql body must end with END".to_string()))?;
        texts.push(inner.trim().to_string());
    } else {
        let rest = plpgsql_strip_kw(chunks[0].trim(), "begin").ok_or_else(|| {
            unsupported(format!(
                "plpgsql body must start with BEGIN, got: {}",
                chunks[0].trim().chars().take(40).collect::<String>()
            ))
        })?;
        texts.push(rest.trim().to_string());
        for c in &chunks[1..n - 1] {
            texts.push(c.trim().to_string());
        }
        let inner = plpgsql_strip_end(&chunks[n - 1])
            .ok_or_else(|| syntax("plpgsql body must end with END".to_string()))?;
        texts.push(inner.trim().to_string());
    }
    let mut stmts: Vec<PlpgsqlStmt> = Vec::new();
    let mut handlers: Vec<PlpgsqlHandler> = Vec::new();
    let mut cur_handler: Option<PlpgsqlHandler> = None;
    let mut in_exception = false;
    // v1.03: FOR..LOOP spans `;`-separated chunks, so walk by index and
    // accumulate loop chunks until one ends with END LOOP.
    let mut i = 0;
    while i < texts.len() {
        let t = texts[i].trim();
        if !t.is_empty() && plpgsql_starts_with_kw(t, "for") {
            let mut buf = String::new();
            let mut j = i;
            let mut closed = false;
            while j < texts.len() {
                if !buf.is_empty() {
                    buf.push_str(";\n");
                }
                buf.push_str(texts[j].trim());
                if plpgsql_ends_with_end_loop(texts[j].trim()) {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if !closed {
                return Err(syntax("FOR loop without END LOOP".to_string()));
            }
            let stmt = parse_plpgsql_for(&buf)?;
            if in_exception {
                let h = cur_handler
                    .as_mut()
                    .ok_or_else(|| syntax("expected WHEN after EXCEPTION".to_string()))?;
                plpgsql_push_stmt(&mut h.stmts, stmt, "WHEN handler")?;
            } else {
                plpgsql_push_stmt(&mut stmts, stmt, "body")?;
            }
            i = j + 1;
            continue;
        }
        if !in_exception {
            if let Some(after) = plpgsql_strip_kw(t, "exception") {
                in_exception = true;
                let after = after.trim();
                if after.is_empty() {
                    i += 1;
                    continue;
                }
                let w = plpgsql_strip_kw(after, "when")
                    .ok_or_else(|| syntax("expected WHEN after EXCEPTION".to_string()))?;
                cur_handler = Some(parse_plpgsql_when(w)?);
                i += 1;
                continue;
            }
            if t.is_empty() {
                i += 1;
                continue;
            }
            if plpgsql_starts_with_kw(t, "when") {
                return Err(syntax("WHEN outside EXCEPTION section".to_string()));
            }
            let stmt = parse_plpgsql_stmt(t)?;
            plpgsql_push_stmt(&mut stmts, stmt, "body")?;
        } else {
            if t.is_empty() {
                i += 1;
                continue;
            }
            if plpgsql_starts_with_kw(t, "exception") {
                return Err(syntax("duplicate EXCEPTION section".to_string()));
            }
            if let Some(w) = plpgsql_strip_kw(t, "when") {
                if let Some(h) = cur_handler.take() {
                    handlers.push(h);
                }
                cur_handler = Some(parse_plpgsql_when(w)?);
                i += 1;
                continue;
            }
            let h = cur_handler
                .as_mut()
                .ok_or_else(|| syntax("expected WHEN after EXCEPTION".to_string()))?;
            let stmt = parse_plpgsql_stmt(t)?;
            plpgsql_push_stmt(&mut h.stmts, stmt, "WHEN handler")?;
        }
        i += 1;
    }
    if let Some(h) = cur_handler.take() {
        handlers.push(h);
    }
    if in_exception && handlers.is_empty() {
        return Err(syntax("EXCEPTION section without WHEN clause".to_string()));
    }
    // v1.03: validate RETURN / RETURN NEXT against the declared shape,
    // and that every FOR variable / := target is DECLAREd.
    plpgsql_validate_returns(&stmts, &handlers, returns_set)?;
    plpgsql_validate_vars(&decls, &stmts, &handlers)?;
    // v1.03: a SETOF function returns its accumulated rows; no dummy
    // RETURN is appended (PG19 `add_dummy_return` only applies to
    // scalar functions).
    if !returns_set {
        plpgsql_ensure_trailing_return(&mut stmts)?;
        for h in &mut handlers {
            plpgsql_ensure_trailing_return(&mut h.stmts)?;
        }
    }
    Ok(PlpgsqlBody {
        decls,
        stmts,
        handlers,
    })
}

/// v1.00: parse a bounded trigger-function body (`RETURNS trigger`,
/// `LANGUAGE plpgsql`) into [`TriggerBodyStmt`]s. The grammar is
/// deliberately small — `BEGIN <stmt>; ... END` where each statement is
/// one of:
/// - `NEW.<col> = <expr>` or `NEW.<col> := <expr>` (assignment)
/// - `RETURN NEW` | `RETURN OLD` | `RETURN NULL`
/// - `RAISE NOTICE '<format>' [, <expr> ...]` |
///   `RAISE EXCEPTION '<format>' [, <expr> ...]`
///
/// Expressions reuse the main SQL expression parser. Anything else is a
/// 42601 syntax error (like PG's plpgsql validator at CREATE time).
pub fn parse_trigger_body(body: &str) -> Result<Vec<TriggerBodyStmt>, SqlError> {
    let tokens = tokenize(body)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        unnamed_seq: 0,
        allow_similar_to: true,
    };
    p.parse_trigger_body_stmts()
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

// ---------------------------------------------------------------------------
// v1.00: triggers (PG19 `CreateTrigStmt`, bounded). BEFORE INSERT
// FOR EACH ROW triggers fire in the executor (NEW assignment,
// RETURN NEW/NULL, RAISE NOTICE); AFTER triggers and other events are
// parsed and cataloged but never fired (documented gap).
// ---------------------------------------------------------------------------

/// v1.00: trigger timing (PG19 `TRIGGER_BEFORE` / `TRIGGER_AFTER`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
}

/// v1.00: trigger event bitmask (PG19 `TRIGGER_INSERT` etc., simplified
/// to one bit per event so it fits in a single WAL/checkpoint byte).
pub mod trig_event {
    /// INSERT event.
    pub const INSERT: u8 = 1;
    /// DELETE event.
    pub const DELETE: u8 = 2;
    /// UPDATE event.
    pub const UPDATE: u8 = 4;
    /// TRUNCATE event (parsed; never fired in v1.00).
    pub const TRUNCATE: u8 = 8;
}

/// v1.00: a trigger definition as parsed from `CREATE TRIGGER`. Stored
/// on the table's catalog entry (`storage::Table.triggers`) so it
/// versions, WAL-replays, and checkpoints with the table. `args` are
/// the `EXECUTE FUNCTION f(args)` argument source texts (debug format);
/// v1.00 accepts them syntactically but does not pass them to the
/// function (no `TG_ARGV`).
#[derive(Clone, Debug)]
pub struct TriggerDef {
    pub name: String,
    pub timing: TriggerTiming,
    pub events: u8,
    pub for_each_row: bool,
    pub function: String,
    /// v1.00: accepted syntactically, not passed to the function (no
    /// `TG_ARGV`). Kept so the DDL round-trips faithfully.
    #[allow(dead_code)]
    pub args: Vec<String>,
}

/// v1.00: one statement of a bounded trigger-function body
/// (`RETURNS trigger`, `LANGUAGE plpgsql`). This is a separate,
/// deliberately small grammar — not an expansion of the bounded
/// PL/pgSQL function body (`desugar_plpgsql_body`): it supports exactly
/// what BEFORE ROW triggers need.
#[derive(Clone, Debug)]
pub enum TriggerBodyStmt {
    /// `NEW.col = <expr>` or `NEW.col := <expr>`.
    Assign { col: String, expr: Expr },
    /// `RETURN NEW`.
    ReturnNew,
    /// `RETURN OLD`.
    ReturnOld,
    /// `RETURN NULL` — the row is skipped (like PG19).
    ReturnNull,
    /// `RAISE <level> '<format>' [, <expr> ...]`.
    Raise {
        level: RaiseLevel,
        format: String,
        args: Vec<Expr>,
    },
}

/// v1.00: `RAISE` levels supported in trigger bodies. Anything else
/// (DEBUG, LOG, INFO, WARNING) is rejected at parse time with 0A000.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseLevel {
    Notice,
    Exception,
}

/// v1.38: `CREATE CAST` coercion method (PG19 `CoercionMethod`).
/// Only `Binary` (`WITHOUT FUNCTION`) is executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastMethod {
    Binary,
    Function,
    InOut,
}

/// v1.38: `CREATE CAST` coercion context (PG19 `CoercionContext`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastContext {
    Implicit,
    Assignment,
    Explicit,
}

/// v1.45: PG19 EXPLAIN option set, mirroring `ExplainState` fields consumed
/// by `explain_state.c`'s `ParseExplainOptionList`. Only `analyze`/`costs`
/// affect plan output today; every other option is parsed and validated
/// exactly like PG19 (including the options PG19 accepts but we do not
/// render yet) and stored here for future versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainOpts {
    pub verbose: bool,
    pub buffers: bool,
    pub wal: bool,
    pub timing: bool,
    pub summary: bool,
    pub settings: bool,
    pub memory: bool,
    pub generic_plan: bool,
    pub io: bool,
    pub serialize: ExplainSerialize,
    pub format: ExplainFormat,
}

impl Default for ExplainOpts {
    fn default() -> Self {
        ExplainOpts {
            verbose: false,
            buffers: false,
            wal: false,
            timing: false,
            summary: false,
            settings: false,
            memory: false,
            generic_plan: false,
            io: false,
            serialize: ExplainSerialize::None,
            format: ExplainFormat::Text,
        }
    }
}

/// v1.45: PG19 `EXPLAIN (SERIALIZE ...)` value (`explain_state.c`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainSerialize {
    None,
    Text,
    Binary,
}

/// v1.45: PG19 `EXPLAIN (FORMAT ...)` value (`explain_state.c`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainFormat {
    Text,
    Xml,
    Json,
    Yaml,
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
        // v0.98: IF EXISTS was parsed but dropped before; now honored
        // (missing sequence -> NOTICE, like PG19).
        if_exists: bool,
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
    // --- v1.38: CREATE CAST (bounded): only the WITHOUT FUNCTION
    // (binary) method is executed; WITH FUNCTION / WITH INOUT parse
    // and are honestly rejected at execution time (0A000).
    CreateCast {
        src: String,
        dst: String,
        method: CastMethod,
        context: CastContext,
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
    // --- v0.97: ALTER DOMAIN (PG19 AlterDomainStmt, bounded) ---
    AlterDomain {
        name: String,
        action: AlterDomainAction,
    },
    /// v1.09: `ALTER FUNCTION name(argtypes) {VOLATILE|STABLE|IMMUTABLE}`
    /// (PG19 AlterFunctionStmt, volatility action only).
    AlterFunction {
        name: String,
        arg_types: Vec<String>,
        volatility: FuncVolatility,
    },
    // --- v0.86: CREATE FUNCTION (bounded): SQL-language and internal
    // functions, plus bounded plpgsql (v0.97: single-RETURN bodies
    // desugared to SQL at CREATE). Only `LANGUAGE sql`, `LANGUAGE
    // plpgsql` and `LANGUAGE internal` are supported; any other
    // language (C, ...) is rejected at parse time with 42601. `args` carries the optional argument
    // names (for named references like `t.col` in the body) and the
    // declared type names. `body` is the raw function-body string;
    // the executor parses it once at CREATE time. `or_replace`
    // implements CREATE OR REPLACE (42723 without it on duplicates).
    // v1.32: `lang_name` is the raw LANGUAGE name as written (PG19
    // parses any name; the executor resolves it, 42883 if unknown).
    CreateFunction {
        name: String,
        args: Vec<FuncArg>,
        ret_type: String,
        returns_set: bool,
        lang_name: String,
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
    // --- v1.00: CREATE TRIGGER (bounded, PG19 `CreateTrigStmt`) ---
    CreateTrigger {
        name: String,
        table: String,
        timing: TriggerTiming,
        /// Bitmask of `trig_event::*`.
        events: u8,
        for_each_row: bool,
        function: String,
        /// `EXECUTE FUNCTION f(args)` argument source texts.
        args: Vec<String>,
        /// v1.00: `WHEN (...)` is parsed; the executor does not
        /// evaluate it (documented gap).
        when: Option<Expr>,
        /// `CONSTRAINT` triggers are cataloged; enforcement is a gap.
        is_constraint: bool,
    },
    /// v1.00: `DROP TRIGGER [IF EXISTS] name ON table [CASCADE|RESTRICT]`.
    DropTrigger {
        name: String,
        table: String,
        if_exists: bool,
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
    // v1.03: `analyze` preserves the ANALYZE option; true means the inner
    // SELECT is executed once and actual row counts are rendered.
    // v1.08: `costs` preserves the COSTS option (default true, like
    // Postgres); false omits the `(rows=N)` estimate suffix from every
    // node line in the planning-only text renderer.
    // v1.45: `opts` carries the full PG19 option set, parsed and validated
    // per explain_state.c ParseExplainOptionList. Only analyze/costs affect
    // output today; the rest are accepted (or rejected exactly like PG19)
    // and stored for future versions.
    Explain {
        stmt: Box<Stmt>,
        analyze: bool,
        costs: bool,
        opts: ExplainOpts,
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
        /// v1.07: `SET TRANSACTION SNAPSHOT 'snapshot-id'` — imported
        /// snapshot id (PG19 special syntax, not combinable with modes).
        snapshot: Option<String>,
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
                | Stmt::CreateCast { .. }
                | Stmt::CreateDomain { .. }
                | Stmt::AlterDomain { .. }
                | Stmt::DropDomain { .. }
                | Stmt::AlterFunction { .. }
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
            Stmt::Explain { stmt, .. } => stmt.max_param(),
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
pub(crate) fn max_param_ctes(ctes: &[CteDef]) -> usize {
    let mut m = 0;
    for c in ctes {
        match &c.body {
            CteBody::Simple(s) => m = m.max(max_param_select(s)),
            CteBody::Union { left, right, .. } => {
                m = m.max(max_param_select(left).max(max_param_select(right)));
            }
            // v1.39: params inside a data-modifying CTE body.
            CteBody::Dml(stmt) => m = m.max(stmt.max_param()),
        }
    }
    m
}

/// v0.10: params inside `ON CONFLICT ... DO UPDATE`.
pub(crate) fn max_param_on_conflict(oc: &OnConflict) -> usize {
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
pub(crate) fn max_param_returning(items: &[SelectItem]) -> usize {
    let mut m = 0;
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            m = m.max(max_param_expr(expr));
        }
    }
    m
}

pub(crate) fn max_param_select(s: &SelectStmt) -> usize {
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

pub(crate) fn max_param_from(f: &FromItem) -> usize {
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

pub(crate) fn max_param_expr(e: &Expr) -> usize {
    match e {
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => max_param_expr(expr),
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
        // v1.30: ordered-set aggregate — params may hide in direct
        // args, the WITHIN GROUP sort keys, or the FILTER.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => direct_args
            .iter()
            .map(max_param_expr)
            .chain(within_order_by.iter().map(|o| max_param_expr(&o.expr)))
            .chain(filter.as_deref().map(max_param_expr))
            .max()
            .unwrap_or(0),
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
pub(crate) fn is_reserved(word: &str) -> bool {
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
pub(crate) fn cte_width(s: &SelectStmt) -> Option<usize> {
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
pub(crate) struct WindowSpec {
    pub(crate) partition_by: Vec<Expr>,
    pub(crate) order_by: Vec<OrderTerm>,
    pub(crate) frame: WindowFrame,
    /// v1.31: PG19 `opt_window_exclusion_clause`.
    pub(crate) exclusion: FrameExclusion,
}

/// v0.88: render a token slice back to SQL-ish source text, for stored
/// index expressions / partial-index predicates (catalog fidelity
/// only — never re-parsed for evaluation). Spacing is canonical, not
/// verbatim: `a * a`, `id1 % 1000 = 1`.
pub(crate) fn tokens_to_sql(toks: &[Token]) -> String {
    fn text(t: &Token) -> String {
        match t {
            Token::Ident(s) => s.clone(),
            Token::QIdent(s) => format!("\"{}\"", s.replace('"', "\"\"")),
            Token::Number(s) => s.clone(),
            Token::Str(s) => format!("'{}'", s.replace('\'', "''")),
            // v1.39: render a bit-string literal back in `x'...'` form
            // (catalog fidelity only).
            Token::BitStr(marker, s) => format!("{}'{}'", marker, s),
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
            Token::FatArrow => "=>".to_string(),
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
