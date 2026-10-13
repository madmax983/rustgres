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

/// Evaluate `left OP right` for the JSON operator family (v1.82
/// extraction; v1.83 adds the jsonb containment/existence operators).
///
/// A `json` document is carried as `Value::Text` (since v0.73); a
/// `jsonb` value is `Value::Jsonb` holding canonical text (v1.83) and
/// dispatches to the tree-based jsonb evaluation. A NULL on either
/// side -> SQL NULL. An unparseable document -> SQL NULL: our `::json`
/// cast does not validate on input, so PG's input-time 22P02 cannot
/// fire here; NULL is the conservative choice.
///
/// Deviation from PG19, documented: PG resolves the extraction
/// operators only for `json`/`jsonb`, but we cannot distinguish `json`
/// from `text` at eval time (both are `Value::Text`), so a `text`
/// value holding JSON is also accepted. The containment/existence
/// family IS jsonb-only in PG and here: it requires a `Value::Jsonb`
/// left operand (a text/json left operand is 42883, as in PG).
pub(crate) fn eval_json_op(op: JsonOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    if left == &Value::Null || right == &Value::Null {
        return Ok(Value::Null);
    }
    if let Value::Jsonb(s) = left {
        return eval_jsonb_op(op, s, right);
    }
    // v1.83: a bare literal left operand of the containment/existence
    // family resolves to jsonb in PG (unknown -> jsonb). A text value
    // holding valid JSON takes the same path (the eval-time type
    // blindness documented above); non-JSON text is 42883, as in PG.
    if op.returns_bool() {
        if let Value::Text(s) | Value::BpChar(s) = left {
            if parse_jsonb(s).is_ok() {
                return eval_jsonb_op(op, s, right);
            }
        }
    }
    if op.returns_bool() {
        return Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                value_type_name(left),
                op.sql(),
                value_type_name(right)
            ),
        ));
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
        // v1.83: the containment/existence family is jsonb-only;
        // eval_json_op rejects it for text/json operands before this
        // point, so this arm is defensive.
        JsonOp::Contains
        | JsonOp::ContainedBy
        | JsonOp::Exists
        | JsonOp::ExistsAny
        | JsonOp::ExistsAll => {
            return Err(exec_err(
                "42883",
                format!(
                    "operator does not exist: json {} {}",
                    op.sql(),
                    value_type_name(right)
                ),
            ));
        }
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
            // -> SQL NULL. v1.83: bare literals coerce via
            // json_path_elems.
            let path = match json_path_elems(op, "json", right)? {
                None => return Ok(Value::Null),
                Some(p) => p,
            };
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

// ---------------------------------------------------------------------------
// v1.83: jsonb — canonical value model, comparison, containment.
//
// Grounded in PG19 `src/backend/utils/adt/jsonb.c` (jsonb_in /
// jsonb_in_scalar / JsonbToCStringWorker), `jsonb_util.c`
// (lengthCompareJsonbString, uniqueifyJsonbObject,
// compareJsonbContainers, compareJsonbScalarValue), `json.c`
// (escape_json_char), and `src/common/jsonapi.c` (string decoding).
// A jsonb value rides in `Value::Jsonb` as its canonical text; the
// functions here parse that text into a tree on demand.
// ---------------------------------------------------------------------------

use std::cmp::Ordering;

/// A canonical jsonb value tree. Object pairs are sorted by the PG key
/// order (byte length, then bytewise) with duplicates removed
/// (last wins) — the invariant `jsonb_from_node` establishes and every
/// consumer may rely on.
#[derive(Clone, Debug)]
pub(crate) enum JsonbVal {
    Null,
    Bool(bool),
    Number(Numeric),
    Str(String),
    Array(Vec<JsonbVal>),
    Object(Vec<(String, JsonbVal)>),
}

/// PG's jsonb object-key order (jsonb_util.c
/// `lengthCompareJsonbString`): shorter keys first, ties broken by
/// bytewise memcmp of the UTF-8 bytes.
pub(crate) fn jsonb_key_cmp(a: &str, b: &str) -> Ordering {
    if a.len() == b.len() {
        a.as_bytes().cmp(b.as_bytes())
    } else {
        a.len().cmp(&b.len())
    }
}

/// Convert a parsed JSON document tree into a canonical jsonb value
/// (jsonb.c's parse-to-JsonbValue conversion): object keys sorted and
/// deduplicated last-wins (jsonb_util.c `uniqueifyJsonbObject`), number
/// tokens converted through PG `numeric_in` semantics (our
/// `Numeric::parse`), strings checked for the one code point PG cannot
/// store (jsonapi.c: "\u0000 cannot be converted to text.").
pub(crate) fn jsonb_from_node(doc: &JsonDoc, node: &JsonNode) -> Result<JsonbVal, ExecError> {
    match &node.kind {
        JsonKind::Null => Ok(JsonbVal::Null),
        // JsonKind::Bool drops the value; the span text has it.
        JsonKind::Bool => Ok(JsonbVal::Bool(node_text(doc, node) == "true")),
        JsonKind::Number => {
            let tok = node_text(doc, node);
            match Numeric::parse(tok) {
                Ok(n) => Ok(JsonbVal::Number(n)),
                Err(crate::storage::NumericParseError::Overflow) => {
                    Err(exec_err("22003", "value overflows numeric format"))
                }
                Err(_) => Err(exec_err("22P02", "invalid input syntax for type jsonb")),
            }
        }
        JsonKind::Str(s) => {
            if s.contains('\0') {
                return Err(exec_err("22P02", "\\u0000 cannot be converted to text."));
            }
            Ok(JsonbVal::Str(s.clone()))
        }
        JsonKind::Array(elems) => {
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                out.push(jsonb_from_node(doc, e)?);
            }
            Ok(JsonbVal::Array(out))
        }
        JsonKind::Object(fields) => {
            let mut pairs: Vec<(usize, String, JsonbVal)> = Vec::with_capacity(fields.len());
            for (i, (k, v)) in fields.iter().enumerate() {
                if k.contains('\0') {
                    return Err(exec_err("22P02", "\\u0000 cannot be converted to text."));
                }
                pairs.push((i, k.clone(), jsonb_from_node(doc, v)?));
            }
            // PG sorts by key; equal keys are ordered by DESCENDING
            // document order so the unique pass keeps the LAST value.
            pairs.sort_by(|a, b| jsonb_key_cmp(&a.1, &b.1).then(b.0.cmp(&a.0)));
            let mut out: Vec<(String, JsonbVal)> = Vec::with_capacity(pairs.len());
            for (_, k, v) in pairs {
                if out.last().is_some_and(|(pk, _)| *pk == k) {
                    continue;
                }
                out.push((k, v));
            }
            Ok(JsonbVal::Object(out))
        }
    }
}

/// Validate + canonicalize JSON text into a jsonb value tree
/// (PG's `jsonb_in`). Malformed input is 22P02.
pub(crate) fn parse_jsonb(text: &str) -> Result<JsonbVal, ExecError> {
    let doc = parse_json(text).map_err(|_| {
        exec_err(
            "22P02",
            format!("invalid input syntax for type jsonb: {:?}", text),
        )
    })?;
    jsonb_from_node(&doc, &doc.root)
}

/// The canonical text of a jsonb value (PG's `jsonb_in` result).
pub(crate) fn jsonb_canonical(text: &str) -> Result<String, ExecError> {
    Ok(parse_jsonb(text)?.render())
}

/// Render in PG's `jsonb_out` format (JsonbToCStringWorker,
/// non-indent): `{"k": v, "k2": v2}`, `[a, b]`, strings escaped per
/// `escape_json_char`, numbers via `numeric_out`.
pub(crate) fn jsonb_render(v: &JsonbVal, out: &mut String) {
    match v {
        JsonbVal::Null => out.push_str("null"),
        JsonbVal::Bool(true) => out.push_str("true"),
        JsonbVal::Bool(false) => out.push_str("false"),
        JsonbVal::Number(n) => out.push_str(&n.to_text()),
        JsonbVal::Str(s) => jsonb_escape_string(s, out),
        JsonbVal::Array(elems) => {
            out.push('[');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                jsonb_render(e, out);
            }
            out.push(']');
        }
        JsonbVal::Object(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                jsonb_escape_string(k, out);
                out.push_str(": ");
                jsonb_render(val, out);
            }
            out.push('}');
        }
    }
}

/// PG19 `escape_json_with_len` / `escape_json_char` (json.c): only
/// `"`, `\`, and control characters below 0x20 are escaped; \b \f \n
/// \r \t have letter forms, the rest are \u00xx (lowercase hex).
/// Non-ASCII passes through as raw UTF-8.
pub(crate) fn jsonb_escape_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl JsonbVal {
    /// The canonical text (PG `jsonb_out`).
    pub(crate) fn render(&self) -> String {
        let mut s = String::new();
        jsonb_render(self, &mut s);
        s
    }

    /// `->` field lookup on an object (exact key match).
    pub(crate) fn field(&self, key: &str) -> Option<&JsonbVal> {
        match self {
            JsonbVal::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `->` index lookup on an array; negative counts from the end
    /// (PG's jsonb_array_element).
    pub(crate) fn index(&self, idx: i64) -> Option<&JsonbVal> {
        match self {
            JsonbVal::Array(elems) => {
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

    /// `#>` path traversal (PG's get_path_all for jsonb): integer-like
    /// elements act as array indexes, others as field names.
    pub(crate) fn path(&self, path: &[String]) -> Option<&JsonbVal> {
        let mut cur = self;
        for elem in path {
            let as_int = if !elem.is_empty() {
                elem.parse::<i64>().ok()
            } else {
                None
            };
            cur = match as_int {
                Some(i) => cur.index(i).or_else(|| cur.field(elem))?,
                None => cur.field(elem)?,
            };
        }
        Some(cur)
    }

    /// `->>`/`#>>` text conversion (PG's jsonb ->> rules): JSON null
    /// -> None (SQL NULL); strings de-escaped; numbers/bools as their
    /// text; containers as their canonical rendering.
    pub(crate) fn scalar_text(&self) -> Option<String> {
        match self {
            JsonbVal::Null => None,
            JsonbVal::Str(s) => Some(s.clone()),
            v => Some(v.render()),
        }
    }
}

/// PG's jsonb scalar type rank for the total order (jsonb.h jbv*:
/// Null=0x0, String, Numeric, Bool, Array=0x10, Object).
fn jsonb_type_rank(v: &JsonbVal) -> u8 {
    match v {
        JsonbVal::Null => 0,
        JsonbVal::Str(_) => 1,
        JsonbVal::Number(_) => 2,
        JsonbVal::Bool(_) => 3,
        JsonbVal::Array(_) => 0x10,
        JsonbVal::Object(_) => 0x11,
    }
}

/// PG's jsonb total order (jsonb_util.c `compareJsonbContainers` over
/// the iterator token stream): type rank first; strings bytewise
/// (varstr_cmp under the C collation); numbers as numerics; bools
/// false < true; arrays compare element count, then elements pairwise;
/// objects compare pair count, then key/value pairs in canonical key
/// order. Equality (`Ordering::Equal`) is exactly PG jsonb equality:
/// key order and numeric scale are irrelevant (1.0 = 1.00).
///
/// Documented deviation: PG wraps top-level scalars in a raw-scalar
/// pseudo-array, which makes a scalar compare GREATER than a shorter
/// real array in one corner (e.g. `5` vs `[]`); we compare scalars by
/// type rank directly (scalar < any array), matching the documented
/// jsonb ordering everywhere except that corner.
pub(crate) fn jsonb_cmp(a: &JsonbVal, b: &JsonbVal) -> Ordering {
    let (ra, rb) = (jsonb_type_rank(a), jsonb_type_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (JsonbVal::Null, JsonbVal::Null) => Ordering::Equal,
        (JsonbVal::Str(x), JsonbVal::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (JsonbVal::Number(x), JsonbVal::Number(y)) => x.cmp(y),
        (JsonbVal::Bool(x), JsonbVal::Bool(y)) => x.cmp(y),
        (JsonbVal::Array(x), JsonbVal::Array(y)) => {
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
            for (ea, eb) in x.iter().zip(y.iter()) {
                let o = jsonb_cmp(ea, eb);
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }
        (JsonbVal::Object(x), JsonbVal::Object(y)) => {
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
            for ((ka, va), (kb, vb)) in x.iter().zip(y.iter()) {
                // PG compares keys LEXICALLY here (varstr_cmp), not by
                // the (length, bytes) storage order.
                let o = ka.as_bytes().cmp(kb.as_bytes());
                if o != Ordering::Equal {
                    return o;
                }
                let o = jsonb_cmp(va, vb);
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }
        _ => Ordering::Equal, // same rank implies same variant
    }
}

/// PG jsonb semantic equality (the `=` operator): total-order equality.
pub(crate) fn jsonb_eq(a: &JsonbVal, b: &JsonbVal) -> bool {
    jsonb_cmp(a, b) == Ordering::Equal
}

/// Semantic grouping key for a jsonb value (used by `value_key` for
/// DISTINCT / GROUP BY / set ops / hash joins): a structural walk with
/// numbers keyed by numeric VALUE (so 1.0 and 1.00 group together,
/// like PG's jsonb hash) and length-prefixed strings/keys, making the
/// encoding injective over distinct semantic values.
pub(crate) fn jsonb_semantic_key(v: &JsonbVal, out: &mut Vec<u8>) {
    match v {
        JsonbVal::Null => out.push(30),
        JsonbVal::Bool(b) => {
            out.push(31);
            out.push(*b as u8);
        }
        JsonbVal::Number(n) => {
            out.push(32);
            value_key_numeric(out, n);
        }
        JsonbVal::Str(s) => {
            out.push(33);
            out.extend_from_slice(&(s.len() as u64).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        JsonbVal::Array(elems) => {
            out.push(34);
            out.extend_from_slice(&(elems.len() as u64).to_be_bytes());
            for e in elems {
                jsonb_semantic_key(e, out);
            }
        }
        JsonbVal::Object(pairs) => {
            out.push(35);
            out.extend_from_slice(&(pairs.len() as u64).to_be_bytes());
            for (k, val) in pairs {
                out.extend_from_slice(&(k.len() as u64).to_be_bytes());
                out.extend_from_slice(k.as_bytes());
                jsonb_semantic_key(val, out);
            }
        }
    }
}

/// PG's `@>` containment (jsonb_util.c JsonbDeepContains semantics):
/// - object @> object: every RHS key exists in LHS and its value
///   recursively contains the RHS value;
/// - array @> array: every RHS element is contained in some LHS
///   element;
/// - array @> scalar: some element recursively contains the scalar
///   (nested arrays are searched);
/// - scalar @> scalar: jsonb equality;
/// - anything else: false.
pub(crate) fn jsonb_contains(a: &JsonbVal, b: &JsonbVal) -> bool {
    match b {
        JsonbVal::Object(bpairs) => match a {
            JsonbVal::Object(apairs) => bpairs.iter().all(|(bk, bv)| {
                apairs
                    .iter()
                    .find(|(ak, _)| ak == bk)
                    .is_some_and(|(_, av)| jsonb_contains(av, bv))
            }),
            _ => false,
        },
        JsonbVal::Array(belems) => match a {
            JsonbVal::Array(aelems) => belems
                .iter()
                .all(|be| aelems.iter().any(|ae| jsonb_contains(ae, be))),
            _ => false,
        },
        _ => match a {
            JsonbVal::Array(aelems) => aelems.iter().any(|ae| jsonb_contains(ae, b)),
            JsonbVal::Object(_) => false,
            _ => jsonb_eq(a, b),
        },
    }
}

/// PG's `?` existence: on an object, key presence; on an array,
/// whether any element is the given string; else false.
pub(crate) fn jsonb_exists(a: &JsonbVal, key: &str) -> bool {
    match a {
        JsonbVal::Object(pairs) => pairs.iter().any(|(k, _)| k == key),
        JsonbVal::Array(elems) => elems
            .iter()
            .any(|e| matches!(e, JsonbVal::Str(s) if s == key)),
        _ => false,
    }
}

/// PG's `||` for jsonb (jsonfuncs.c jsonb_concat / JsonbConcat):
/// object||object merges (right wins); array||array concatenates;
/// array||other appends; other||array prepends; two scalars/objects
/// become a two-element array. Object merge re-sorts canonically.
pub(crate) fn jsonb_concat(a: &JsonbVal, b: &JsonbVal) -> JsonbVal {
    match (a, b) {
        (JsonbVal::Object(ap), JsonbVal::Object(bp)) => {
            let mut out: Vec<(String, JsonbVal)> = ap.clone();
            for (bk, bv) in bp {
                if let Some(slot) = out.iter_mut().find(|(ak, _)| ak == bk) {
                    slot.1 = bv.clone();
                } else {
                    out.push((bk.clone(), bv.clone()));
                }
            }
            out.sort_by(|x, y| jsonb_key_cmp(&x.0, &y.0));
            JsonbVal::Object(out)
        }
        (JsonbVal::Array(ae), JsonbVal::Array(be)) => {
            let mut out = ae.clone();
            out.extend(be.iter().cloned());
            JsonbVal::Array(out)
        }
        (JsonbVal::Array(ae), other) => {
            let mut out = ae.clone();
            out.push(other.clone());
            JsonbVal::Array(out)
        }
        (other, JsonbVal::Array(be)) => {
            let mut out = vec![other.clone()];
            out.extend(be.iter().cloned());
            JsonbVal::Array(out)
        }
        (x, y) => JsonbVal::Array(vec![x.clone(), y.clone()]),
    }
}

/// The `#>`/`#>>` path elements from the right operand. PG types the
/// operand as text[]; an untyped literal (`'{a,0}'`) is coerced by
/// parsing it as an array literal. A non-array, non-literal operand is
/// 42883. Shared by the json and jsonb paths (v1.83: v1.82 required an
/// already-array value, which the corpus's bare-literal paths missed).
fn json_path_elems(
    op: JsonOp,
    left_name: &str,
    right: &Value,
) -> Result<Option<Vec<String>>, ExecError> {
    let arr = match right {
        Value::Array(a) => (**a).clone(),
        Value::Text(s) | Value::BpChar(s) => match parse_array_literal(s, ArrayElem::Text) {
            Ok(a) => a,
            Err(_) => {
                return Err(exec_err(
                    "42883",
                    format!(
                        "operator does not exist: {} {} {}",
                        left_name,
                        op.sql(),
                        value_type_name(right)
                    ),
                ));
            }
        },
        other => {
            return Err(exec_err(
                "42883",
                format!(
                    "operator does not exist: {} {} {}",
                    left_name,
                    op.sql(),
                    value_type_name(other)
                ),
            ));
        }
    };
    let mut path = Vec::with_capacity(arr.elems.len());
    for e in &arr.elems {
        match e {
            // PG's get_path_all: any NULL path element -> SQL NULL.
            Value::Null => return Ok(None),
            Value::Text(s) | Value::BpChar(s) => path.push(s.to_string()),
            other => {
                return Err(exec_err(
                    "42883",
                    format!(
                        "operator does not exist: {} {} (non-text path element {})",
                        left_name,
                        op.sql(),
                        value_type_name(other)
                    ),
                ));
            }
        }
    }
    Ok(Some(path))
}

/// Evaluation of the JSON operator family for a `Value::Jsonb` left
/// operand (canonical text). The tree parse cannot fail for values
/// produced by our own casts; a hand-built bad value degrades to NULL
/// like the json path.
fn eval_jsonb_op(op: JsonOp, doc_text: &str, right: &Value) -> Result<Value, ExecError> {
    let doc = match parse_jsonb(doc_text) {
        Ok(d) => d,
        Err(_) => return Ok(Value::Null),
    };
    match op {
        JsonOp::Arrow | JsonOp::ArrowText => {
            let node = match right {
                Value::Text(k) | Value::BpChar(k) => doc.field(k.as_ref()),
                Value::Int(i) | Value::BigInt(i) => doc.index(*i),
                Value::SmallInt(i) => doc.index(*i as i64),
                other => {
                    return Err(exec_err(
                        "42883",
                        format!(
                            "operator does not exist: jsonb {} {}",
                            op.sql(),
                            value_type_name(other)
                        ),
                    ));
                }
            };
            Ok(jsonb_extract_result(op, node))
        }
        JsonOp::HashArrow | JsonOp::HashArrowText => {
            let path = match json_path_elems(op, "jsonb", right)? {
                None => return Ok(Value::Null),
                Some(p) => p,
            };
            Ok(jsonb_extract_result(op, doc.path(&path)))
        }
        JsonOp::Contains | JsonOp::ContainedBy => {
            let other = match right {
                Value::Jsonb(s) => match parse_jsonb(s) {
                    Ok(d) => d,
                    Err(_) => return Ok(Value::Null),
                },
                // PG resolves a bare literal RHS to jsonb (unknown ->
                // jsonb) and the cast runs jsonb_in, so an unparseable
                // literal surfaces the input error (22P02) rather than
                // 42883. (A typed text value would be 42883 in PG; we
                // cannot distinguish the two at eval time.)
                Value::Text(s) | Value::BpChar(s) => parse_jsonb(s)?,
                other => {
                    return Err(exec_err(
                        "42883",
                        format!(
                            "operator does not exist: jsonb {} {}",
                            op.sql(),
                            value_type_name(other)
                        ),
                    ));
                }
            };
            let r = if op == JsonOp::Contains {
                jsonb_contains(&doc, &other)
            } else {
                jsonb_contains(&other, &doc)
            };
            Ok(Value::Bool(r))
        }
        JsonOp::Exists | JsonOp::ExistsAny | JsonOp::ExistsAll => {
            // Key list with NULLs preserved: PG's ?|/?& resolve a
            // decisive match first and yield NULL only when no element
            // decides (jsonfuncs.c jsonb_exists_any/all walk the whole
            // array tracking a null flag).
            let keys: Vec<Option<String>> = match right {
                Value::Text(s) | Value::BpChar(s) if op == JsonOp::Exists => {
                    vec![Some(s.to_string())]
                }
                _ if op != JsonOp::Exists => {
                    // text[] operand; bare literals coerce like #>.
                    let arr = match right {
                        Value::Array(a) => (**a).clone(),
                        Value::Text(s) | Value::BpChar(s) => {
                            match parse_array_literal(s, ArrayElem::Text) {
                                Ok(a) => a,
                                Err(_) => {
                                    return Err(exec_err(
                                        "42883",
                                        format!(
                                            "operator does not exist: jsonb {} {}",
                                            op.sql(),
                                            value_type_name(right)
                                        ),
                                    ));
                                }
                            }
                        }
                        other => {
                            return Err(exec_err(
                                "42883",
                                format!(
                                    "operator does not exist: jsonb {} {}",
                                    op.sql(),
                                    value_type_name(other)
                                ),
                            ));
                        }
                    };
                    let mut ks = Vec::with_capacity(arr.elems.len());
                    for e in &arr.elems {
                        match e {
                            Value::Text(s) | Value::BpChar(s) => ks.push(Some(s.to_string())),
                            Value::Null => ks.push(None),
                            other => {
                                return Err(exec_err(
                                    "42883",
                                    format!(
                                        "operator does not exist: jsonb {} (non-text element {})",
                                        op.sql(),
                                        value_type_name(other)
                                    ),
                                ));
                            }
                        }
                    }
                    ks
                }
                other => {
                    return Err(exec_err(
                        "42883",
                        format!(
                            "operator does not exist: jsonb {} {}",
                            op.sql(),
                            value_type_name(other)
                        ),
                    ));
                }
            };
            let has_null = keys.iter().any(|k| k.is_none());
            match op {
                JsonOp::Exists => Ok(Value::Bool(jsonb_exists(
                    &doc,
                    keys[0].as_deref().unwrap_or(""),
                ))),
                JsonOp::ExistsAny => {
                    if keys.iter().flatten().any(|k| jsonb_exists(&doc, k)) {
                        Ok(Value::Bool(true))
                    } else if has_null {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::Bool(false))
                    }
                }
                _ => {
                    // ExistsAll
                    if keys.iter().flatten().any(|k| !jsonb_exists(&doc, k)) {
                        Ok(Value::Bool(false))
                    } else if has_null {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::Bool(true))
                    }
                }
            }
        }
    }
}

/// Shared `->`/`#>` vs `->>`/`#>>` result shaping for jsonb.
fn jsonb_extract_result(op: JsonOp, node: Option<&JsonbVal>) -> Value {
    match node {
        None => Value::Null,
        Some(n) if op.returns_json() => Value::jsonb(n.render()),
        Some(n) => match n.scalar_text() {
            Some(s) => Value::text(s),
            None => Value::Null,
        },
    }
}

#[cfg(test)]
mod jsonb_tests {
    use super::*;

    fn canon(s: &str) -> String {
        jsonb_canonical(s).unwrap()
    }

    #[test]
    fn canonical_sorts_and_dedupes_keys() {
        // PG: sort by (length, bytewise); last duplicate wins.
        assert_eq!(
            canon(r#"{"bb": 1, "a": 2, "bb": 3, "ab": 4}"#),
            r#"{"a": 2, "ab": 4, "bb": 3}"#
        );
        assert_eq!(canon(r#"{"b":1,"a":2}"#), r#"{"a": 2, "b": 1}"#);
    }

    #[test]
    fn canonical_numbers_via_numeric() {
        assert_eq!(canon("1e2"), "100");
        assert_eq!(canon("1.50"), "1.50");
        assert_eq!(canon("[1.0, 1.00, -0.5e1]"), "[1.0, 1.00, -5]");
        assert_eq!(canon("0.000001"), "0.000001");
    }

    #[test]
    fn canonical_strings_reescaped() {
        // \u escapes decode to raw UTF-8; control chars re-escape.
        assert_eq!(canon(r#""A""#), "\"A\"");
        assert_eq!(canon(r#""\u0041""#), "\"A\"");
        assert_eq!(canon(r#""a\nb""#), r#""a\nb""#);
        assert_eq!(canon(r#""\u0001""#), r#""\u0001""#);
        assert_eq!(canon(r#""é""#), "\"é\"");
        // PG jsonb rejects \u0000 (cannot be converted to text).
        assert!(jsonb_canonical(r#""\u0000""#).is_err());
        assert!(jsonb_canonical("nul").is_err());
    }

    #[test]
    fn canonical_spacing() {
        assert_eq!(
            canon(r#"{"a":[1,2,{"b":true}],"c":null}"#),
            r#"{"a": [1, 2, {"b": true}], "c": null}"#
        );
        assert_eq!(canon("[]"), "[]");
        assert_eq!(canon("{}"), "{}");
        assert_eq!(canon("true"), "true");
    }

    #[test]
    fn jsonb_equality_is_semantic() {
        let a = parse_jsonb(r#"{"a": 1.0, "b": [1, 2]}"#).unwrap();
        let b = parse_jsonb(r#"{"b": [1, 2], "a": 1.00}"#).unwrap();
        assert!(jsonb_eq(&a, &b));
        let c = parse_jsonb(r#"{"a": 1, "b": [1, 2]}"#).unwrap();
        assert!(jsonb_eq(&a, &c)); // 1.0 = 1 numerically
        let d = parse_jsonb(r#"{"a": 2, "b": [1, 2]}"#).unwrap();
        assert!(!jsonb_eq(&a, &d));
    }

    #[test]
    fn jsonb_total_order() {
        // Type rank: null < string < number < bool < array < object.
        let vals = ["null", "\"a\"", "10", "2", "true", "[1]", "{\"a\": 1}"];
        let parsed: Vec<JsonbVal> = vals.iter().map(|s| parse_jsonb(s).unwrap()).collect();
        // "10" vs "2": numeric compare (10 > 2), not lexical.
        assert_eq!(jsonb_cmp(&parsed[2], &parsed[3]), Ordering::Greater);
        assert_eq!(jsonb_cmp(&parsed[0], &parsed[1]), Ordering::Less);
        assert_eq!(jsonb_cmp(&parsed[1], &parsed[2]), Ordering::Less);
        assert_eq!(jsonb_cmp(&parsed[3], &parsed[4]), Ordering::Less);
        assert_eq!(jsonb_cmp(&parsed[4], &parsed[5]), Ordering::Less);
        assert_eq!(jsonb_cmp(&parsed[5], &parsed[6]), Ordering::Less);
        // Arrays: length first.
        let e1 = parse_jsonb("[1, 2]").unwrap();
        let e2 = parse_jsonb("[9]").unwrap();
        assert_eq!(jsonb_cmp(&e1, &e2), Ordering::Greater);
    }

    #[test]
    fn jsonb_containment() {
        let c =
            |a: &str, b: &str| jsonb_contains(&parse_jsonb(a).unwrap(), &parse_jsonb(b).unwrap());
        assert!(c(r#"{"a": {"b": 1}, "c": 2}"#, r#"{"a": {"b": 1}}"#));
        assert!(!c(r#"{"a": 1}"#, r#"{"a": 2}"#));
        assert!(c("[1, [2, 3]]", "[3]"));
        assert!(c("[[1, 2]]", "1")); // deep scalar search in arrays
        assert!(!c(r#"{"a": 1}"#, "1")); // object does not contain scalar
        assert!(c(r#""foo""#, r#""foo""#));
        assert!(c("[1.0]", "[1.00]")); // numeric-aware
    }

    #[test]
    fn jsonb_existence() {
        let d = parse_jsonb(r#"{"a": 1, "b": [1, "x"]}"#).unwrap();
        assert!(jsonb_exists(&d, "a"));
        assert!(!jsonb_exists(&d, "x")); // nested does not count
        let arr = parse_jsonb(r#"["x", 1]"#).unwrap();
        assert!(jsonb_exists(&arr, "x"));
        assert!(!jsonb_exists(&arr, "1")); // numbers are not strings
    }

    #[test]
    fn jsonb_concat_rules() {
        let cc = |a: &str, b: &str| {
            jsonb_concat(&parse_jsonb(a).unwrap(), &parse_jsonb(b).unwrap()).render()
        };
        assert_eq!(
            cc(r#"{"b": 1}"#, r#"{"a": 2, "b": 3}"#),
            r#"{"a": 2, "b": 3}"#
        );
        assert_eq!(cc("[1, 2]", "[3]"), "[1, 2, 3]");
        assert_eq!(cc("[1]", r#"{"a": 1}"#), r#"[1, {"a": 1}]"#);
        assert_eq!(cc(r#"{"a": 1}"#, "[9]"), r#"[{"a": 1}, 9]"#);
        assert_eq!(cc("1", "2"), "[1, 2]");
    }

    #[test]
    fn jsonb_extraction() {
        let d = Value::jsonb(canon(r#"{"a": [10, 20], "b": {"c": "x"}}"#));
        let r = eval_json_op(JsonOp::Arrow, &d, &Value::text("a")).unwrap();
        assert_eq!(r, Value::jsonb("[10, 20]"));
        let r = eval_json_op(JsonOp::ArrowText, &d, &Value::text("b")).unwrap();
        assert_eq!(r, Value::text(r#"{"c": "x"}"#));
        let r = eval_json_op(
            JsonOp::HashArrow,
            &d,
            &Value::Array(Box::new(ArrayVal {
                elem: ArrayElem::Text,
                dims: vec![2],
                lower: vec![1],
                elems: vec![Value::text("b"), Value::text("c")],
            })),
        )
        .unwrap();
        assert_eq!(r, Value::jsonb("\"x\""));
        // Missing key -> SQL NULL.
        let r = eval_json_op(JsonOp::Arrow, &d, &Value::text("zz")).unwrap();
        assert_eq!(r, Value::Null);
    }
}
