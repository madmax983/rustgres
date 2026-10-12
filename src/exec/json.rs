//! JSON document model for the `->`, `->>`, `#>`, `#>>` operators (v1.82).
//!
//! Grounded in PG19 `src/backend/utils/adt/jsonfuncs.c`: `get_worker` and the
//! `json_object_field` / `json_array_element` / `get_path_all` family, which
//! implement these operators for the `json` type. PG streams over the document
//! text with semantic actions; we parse into a tree once per evaluation.
//! Observable semantics match, including:
//!
//! - missing key / out-of-bounds index / wrong container type -> SQL NULL,
//!   never an error (PG's `get_worker` returns NULL for all of these);
//! - negative array indexes count from the end (`jsonb_array_element`);
//! - duplicate object keys resolve to the LAST match (PG's
//!   `get_object_field_start`: "this object overrides any previous matching
//!   object");
//! - `#>`/`#>>` path elements that parse fully as integers act as array
//!   indexes, others as field names; a NULL path element -> SQL NULL; an
//!   empty path returns the whole document (PG's `get_path_all`);
//! - `->>`/`#>>` map JSON null to SQL NULL and de-escape strings
//!   (`normalize_results`);
//! - `->`/`#>` return the value's ORIGINAL source slice, like PG returning
//!   `result_start..token_end` into the input document (whitespace preserved).

use super::*;

/// A parsed JSON document. `text` is the original document; node spans are
/// byte offsets into it so `->`/`#>` can return PG's exact substrings.
pub(crate) struct JsonDoc {
    pub text: String,
    pub root: JsonNode,
}

#[derive(Clone, Debug)]
pub(crate) struct JsonNode {
    pub kind: JsonKind,
    /// Byte span of this value in the source text (start, end).
    pub span: (usize, usize),
}

#[derive(Clone, Debug)]
pub(crate) enum JsonKind {
    Null,
    Bool,
    /// Numbers keep their source text via the node span (no float rounding).
    Number,
    /// Decoded string value (escapes processed).
    Str(String),
    Array(Vec<JsonNode>),
    /// Insertion order; duplicate keys kept, last wins on lookup.
    Object(Vec<(String, JsonNode)>),
}

/// Parse a JSON document. Returns a PG-style error string on invalid input.
pub(crate) fn parse_json(text: &str) -> Result<JsonDoc, String> {
    let mut p = Parser {
        chars: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let root = p.parse_value(text)?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!(
            "unexpected trailing characters at offset {}",
            p.pos
        ));
    }
    Ok(JsonDoc {
        text: text.to_string(),
        root,
    })
}

struct Parser<'a> {
    chars: &'a [u8],
    pos: usize,
    depth: usize,
}

/// Nesting cap for `parse_json`. PG19 bounds nesting via
/// `check_stack_depth` in its recursive-descent JSON parser
/// (src/common/jsonapi.c); 10 000-deep inputs in the PG tests (e.g.
/// `repeat('[', 10000)`) must error, not overflow the stack. Each
/// nesting level costs a few KB of stack in a debug build, so the cap
/// stays at 100 — far above any valid document in the corpus and far
/// below any thread-stack limit.
const MAX_JSON_DEPTH: usize = 100;

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len()
            && matches!(self.chars[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.chars.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        match self.peek() {
            Some(c) if c == b => {
                self.pos += 1;
                Ok(())
            }
            other => Err(format!(
                "expected '{}' at offset {}, found {:?}",
                b as char,
                self.pos,
                other.map(|c| c as char)
            )),
        }
    }

    fn parse_value(&mut self, text: &str) -> Result<JsonNode, String> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return Err("json nesting too deep".to_string());
        }
        let r = self.parse_value_inner(text);
        self.depth -= 1;
        r
    }

    fn parse_value_inner(&mut self, text: &str) -> Result<JsonNode, String> {
        self.skip_ws();
        let start = self.pos;
        let kind = match self.peek() {
            Some(b'n') => {
                self.literal("null")?;
                JsonKind::Null
            }
            Some(b't') => {
                self.literal("true")?;
                JsonKind::Bool
            }
            Some(b'f') => {
                self.literal("false")?;
                JsonKind::Bool
            }
            Some(b'"') => {
                let s = self.parse_string()?;
                JsonKind::Str(s)
            }
            Some(b'[') => {
                self.pos += 1;
                let mut elems = Vec::new();
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                } else {
                    loop {
                        elems.push(self.parse_value(text)?);
                        self.skip_ws();
                        match self.peek() {
                            Some(b',') => {
                                self.pos += 1;
                            }
                            Some(b']') => {
                                self.pos += 1;
                                break;
                            }
                            other => {
                                return Err(format!(
                                    "expected ',' or ']' at offset {}, found {:?}",
                                    self.pos,
                                    other.map(|c| c as char)
                                ));
                            }
                        }
                    }
                }
                JsonKind::Array(elems)
            }
            Some(b'{') => {
                self.pos += 1;
                let mut fields = Vec::new();
                self.skip_ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                } else {
                    loop {
                        self.skip_ws();
                        if self.peek() != Some(b'"') {
                            return Err(format!("expected string key at offset {}", self.pos));
                        }
                        let key = self.parse_string()?;
                        self.skip_ws();
                        self.expect(b':')?;
                        let val = self.parse_value(text)?;
                        fields.push((key, val));
                        self.skip_ws();
                        match self.peek() {
                            Some(b',') => {
                                self.pos += 1;
                            }
                            Some(b'}') => {
                                self.pos += 1;
                                break;
                            }
                            other => {
                                return Err(format!(
                                    "expected ',' or '}}' at offset {}, found {:?}",
                                    self.pos,
                                    other.map(|c| c as char)
                                ));
                            }
                        }
                    }
                }
                JsonKind::Object(fields)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                self.parse_number()?;
                JsonKind::Number
            }
            other => {
                return Err(format!(
                    "expected JSON value at offset {}, found {:?}",
                    self.pos,
                    other.map(|c| c as char)
                ));
            }
        };
        let end = self.pos;
        // Validate UTF-8 span boundaries (source is &str, always valid).
        let _ = &text[start..end];
        Ok(JsonNode {
            kind,
            span: (start, end),
        })
    }

    fn literal(&mut self, lit: &str) -> Result<(), String> {
        for b in lit.bytes() {
            self.expect(b)?;
        }
        Ok(())
    }

    /// Parse a JSON number per RFC 8259; the text stays in the node span.
    fn parse_number(&mut self) -> Result<(), String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("invalid number at offset {}", start)),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!("invalid number at offset {}", start));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!("invalid number at offset {}", start));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        Ok(())
    }

    /// Parse a JSON string, returning the decoded value. `self.pos` is at the
    /// opening quote; advances past the closing quote.
    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".to_string()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{0008}'),
                        Some(b'f') => out.push('\u{000C}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            self.pos += 1;
                            let cp = self.parse_hex4()?;
                            // Surrogate pair?
                            if (0xD800..0xDC00).contains(&cp) {
                                if self.peek() == Some(b'\\') {
                                    let save = self.pos;
                                    self.pos += 1;
                                    if self.peek() == Some(b'u') {
                                        self.pos += 1;
                                        let lo = self.parse_hex4()?;
                                        if (0xDC00..0xE000).contains(&lo) {
                                            let full =
                                                0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            match char::from_u32(full) {
                                                Some(ch) => out.push(ch),
                                                None => {
                                                    return Err(format!(
                                                        "invalid surrogate pair at offset {}",
                                                        save
                                                    ));
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                    self.pos = save;
                                }
                                return Err(format!("unpaired surrogate at offset {}", self.pos));
                            }
                            match char::from_u32(cp) {
                                Some(ch) => out.push(ch),
                                None => {
                                    return Err(format!(
                                        "invalid \\u escape at offset {}",
                                        self.pos
                                    ));
                                }
                            }
                            continue;
                        }
                        other => {
                            return Err(format!(
                                "invalid escape at offset {}, found {:?}",
                                self.pos,
                                other.map(|c| c as char)
                            ));
                        }
                    }
                    self.pos += 1;
                }
                Some(b) => {
                    if b < 0x20 {
                        return Err(format!(
                            "unescaped control character at offset {}",
                            self.pos
                        ));
                    }
                    // Copy the full UTF-8 sequence for this char.
                    let s = std::str::from_utf8(&self.chars[self.pos..])
                        .map_err(|_| "invalid utf-8".to_string())?;
                    let ch = s.chars().next().unwrap();
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let d = match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    self.pos += 1;
                    (c as char).to_digit(16).unwrap()
                }
                _ => return Err(format!("invalid \\u escape at offset {}", self.pos)),
            };
            v = v * 16 + d;
        }
        Ok(v)
    }
}

/// The source slice for a node's value (`->`/`#>` return this, like PG's
/// `result_start..token_end` substring).
pub(crate) fn node_text<'a>(doc: &'a JsonDoc, node: &JsonNode) -> &'a str {
    &doc.text[node.span.0..node.span.1]
}

/// Field lookup: LAST duplicate key wins (PG's `get_object_field_start`).
pub(crate) fn json_field<'a>(node: &'a JsonNode, key: &str) -> Option<&'a JsonNode> {
    match &node.kind {
        JsonKind::Object(fields) => fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

/// Array index: negative counts from the end (PG's `jsonb_array_element`).
/// Out of bounds -> None.
pub(crate) fn json_index(node: &JsonNode, idx: i64) -> Option<&JsonNode> {
    match &node.kind {
        JsonKind::Array(elems) => {
            let len = elems.len() as i64;
            let i = if idx < 0 { len + idx } else { idx };
            if i < 0 || i >= len {
                None
            } else {
                Some(&elems[i as usize])
            }
        }
        _ => None,
    }
}

/// `#>`/`#>>` path traversal (PG's `get_path_all`): each element is tried as
/// an array index if it parses fully as an integer, else as a field name.
/// A missing step -> None.
pub(crate) fn json_path<'a>(node: &'a JsonNode, path: &[String]) -> Option<&'a JsonNode> {
    let mut cur = node;
    for elem in path {
        // PG: "just convert anything in the path that we can to an integer".
        let as_int = if !elem.is_empty() {
            elem.parse::<i64>().ok()
        } else {
            None
        };
        cur = match as_int {
            Some(i) => json_index(cur, i).or_else(|| json_field(cur, elem))?,
            None => json_field(cur, elem)?,
        };
    }
    Some(cur)
}

/// `->>`/`#>>` text conversion (PG's `normalize_results`): JSON null ->
/// None (SQL NULL); strings de-escaped; everything else as source text.
pub(crate) fn json_scalar_text(doc: &JsonDoc, node: &JsonNode) -> Option<String> {
    match &node.kind {
        JsonKind::Null => None,
        JsonKind::Str(s) => Some(s.clone()),
        _ => Some(node_text(doc, node).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scalars() {
        let d = parse_json("null").unwrap();
        assert!(matches!(d.root.kind, JsonKind::Null));
        let d = parse_json("true").unwrap();
        assert!(matches!(d.root.kind, JsonKind::Bool));
        let d = parse_json("  -12.5e+3  ").unwrap();
        assert!(matches!(d.root.kind, JsonKind::Number));
        assert_eq!(node_text(&d, &d.root), "-12.5e+3");
    }

    #[test]
    fn parse_nested_and_lookup() {
        let d = parse_json(r#"{"a": [1, "x", null], "b": {"c": false}}"#).unwrap();
        let a = json_field(&d.root, "a").unwrap();
        assert!(matches!(a.kind, JsonKind::Array(_)));
        assert!(json_index(a, 0).is_some());
        assert!(json_index(a, -1).is_some());
        assert!(json_index(a, 3).is_none());
        assert!(json_index(a, -4).is_none());
        // duplicate keys: last wins
        let d2 = parse_json(r#"{"k": 1, "k": 2}"#).unwrap();
        let k = json_field(&d2.root, "k").unwrap();
        assert_eq!(node_text(&d2, k), "2");
    }

    #[test]
    fn path_and_text() {
        let d = parse_json(r#"{"f2": ["f3", 1]}"#).unwrap();
        let n = json_path(&d.root, &["f2".to_string(), "0".to_string()]).unwrap();
        assert_eq!(json_scalar_text(&d, n).as_deref(), Some("f3"));
        let n = json_path(&d.root, &["f2".to_string(), "1".to_string()]).unwrap();
        assert_eq!(json_scalar_text(&d, n).as_deref(), Some("1"));
        assert!(json_path(&d.root, &["nope".to_string()]).is_none());
        // empty path -> whole document
        let n = json_path(&d.root, &[]).unwrap();
        assert_eq!(node_text(&d, n), r#"{"f2": ["f3", 1]}"#);
    }

    #[test]
    fn escapes() {
        let d = parse_json(r#""a\nb\u0041""#).unwrap();
        match &d.root.kind {
            JsonKind::Str(s) => assert_eq!(s, "a\nbA"),
            _ => panic!(),
        }
    }

    #[test]
    fn accepts_u0000_rejects_deep_nesting() {
        // PG19 accepts the escape at input time (json_encoding.out
        // calls it a "legal escape"); the jsonapi.c CODE_POINT_ZERO
        // rejection fires only on de-escape/convert, out of scope.
        assert!(parse_json("\"\\u0000\"").is_ok());
        // PG19 bounds nesting by stack depth; 10k open brackets error.
        let deep = "[".repeat(10_000);
        assert!(parse_json(&deep).is_err());
        // Moderate nesting is fine.
        let ok = "[".repeat(100) + &"]".repeat(100);
        assert!(parse_json(&ok).is_ok());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_json("{").is_err());
        assert!(parse_json("[1,]").is_err());
        assert!(parse_json("nul").is_err());
        assert!(parse_json("01").is_err());
        assert!(parse_json("").is_err());
    }
}

/// Evaluate `left OP right` for the JSON extraction operators (v1.82).
///
/// The document is carried as `Value::Text` (all `json` values are text
/// since v0.73). A NULL on either side -> SQL NULL. An unparseable document
/// -> SQL NULL: our `::json` cast does not validate on input, so PG's
/// input-time 22P02 cannot fire here; NULL is the conservative choice.
///
/// Deviation from PG19, documented: PG resolves these operators only for
/// `json`/`jsonb`, but we cannot distinguish `json` from `text` at eval
/// time (both are `Value::Text`), so a `text` value holding JSON is also
/// accepted. No conformance test covers the negative case.
pub(crate) fn eval_json_op(op: JsonOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    if left == &Value::Null || right == &Value::Null {
        return Ok(Value::Null);
    }
    let doc_text: &str = match left {
        Value::Text(s) | Value::BpChar(s) => s.as_ref(),
        other => {
            return Err(exec_err(
                "42883",
                format!(
                    "operator does not exist: {} {} {}",
                    value_type_name(other),
                    op.sql(),
                    value_type_name(right)
                ),
            ));
        }
    };
    let doc = match parse_json(doc_text) {
        Ok(d) => d,
        Err(_) => return Ok(Value::Null),
    };
    let as_text = !op.returns_json();
    let node = match op {
        JsonOp::Arrow | JsonOp::ArrowText => {
            // PG19: `json -> text` (field) and `json -> integer` (index)
            // only; anything else is 42883.
            match right {
                Value::Text(k) | Value::BpChar(k) => json_field(&doc.root, k.as_ref()),
                Value::Int(i) | Value::BigInt(i) => json_index(&doc.root, *i),
                Value::SmallInt(i) => json_index(&doc.root, *i as i64),
                other => {
                    return Err(exec_err(
                        "42883",
                        format!(
                            "operator does not exist: json {} {}",
                            op.sql(),
                            value_type_name(other)
                        ),
                    ));
                }
            }
        }
        JsonOp::HashArrow | JsonOp::HashArrowText => {
            // PG19 `get_path_all`: the path is text[]; any NULL element
            // -> SQL NULL.
            let arr = match right {
                Value::Array(a) => a,
                other => {
                    return Err(exec_err(
                        "42883",
                        format!(
                            "operator does not exist: json {} {}",
                            op.sql(),
                            value_type_name(other)
                        ),
                    ));
                }
            };
            let mut path = Vec::with_capacity(arr.elems.len());
            for e in &arr.elems {
                match e {
                    Value::Null => return Ok(Value::Null),
                    Value::Text(s) | Value::BpChar(s) => path.push(s.to_string()),
                    other => {
                        return Err(exec_err(
                            "42883",
                            format!(
                                "operator does not exist: json {} (non-text path element {})",
                                op.sql(),
                                value_type_name(other)
                            ),
                        ));
                    }
                }
            }
            json_path(&doc.root, &path)
        }
    };
    match node {
        None => Ok(Value::Null),
        // `->>`/`#>>`: JSON null -> SQL NULL, strings de-escaped
        // (PG's `normalize_results`).
        Some(n) if as_text => match json_scalar_text(&doc, n) {
            Some(s) => Ok(Value::text(s)),
            None => Ok(Value::Null),
        },
        // `->`/`#>` return the original source slice, like PG's
        // `result_start..token_end` substring.
        Some(n) => Ok(Value::text(node_text(&doc, n))),
    }
}
