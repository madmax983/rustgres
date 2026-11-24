//! v2.42: the XML subsystem — a pure-Rust XML parser/serializer, the
//! `xml` type's input validation, the SQL/XML construction functions,
//! an XPath 1.0 subset evaluator, and XMLTABLE support.
//!
//! Ground truth: PG19 `src/backend/utils/adt/xml.c` + `xpath.c`
//! (local beta3 source). PG stores xml values as their text form and
//! `xml_out` renders that text verbatim, so `Value::Xml` keeps the
//! original input text; the DOM below exists for validation, xpath,
//! xmltable, and for serializing *constructed* values (xmlelement &
//! friends, xpath results), where PG re-serializes through libxml.
//!
//! Namespace model: element/attribute names keep their written
//! (prefixed) form for serialization; each element also records the
//! namespace URI its name resolves to (from in-scope xmlns bindings,
//! `xml` prefix prebound) and the xmlns declarations written on it,
//! because libxml dumps namespace declarations on the element that
//! declares them, not on descendants that merely use them.

use crate::exec::ExecError;
use crate::storage::Value;

/// One XML parse/validation failure. `message` is PG's primary message
/// ("invalid XML document" / "invalid XML content"); `detail` carries
/// the parser's own description (PG puts libxml's text in DETAIL —
/// unreproducible verbatim in pure Rust; the code and primary message
/// are PG-exact).
#[derive(Debug, Clone)]
pub(crate) struct XmlErr {
    pub message: String,
    pub detail: String,
    /// PG19: 2200M for documents, 2200N for content
    /// (ERRCODE_INVALID_XML_DOCUMENT / _CONTENT).
    pub code: &'static str,
}

impl XmlErr {
    fn new(document: bool, detail: impl Into<String>) -> XmlErr {
        XmlErr {
            message: if document {
                "invalid XML document".into()
            } else {
                "invalid XML content".into()
            },
            detail: detail.into(),
            code: if document { "2200M" } else { "2200N" },
        }
    }
    pub fn to_exec(&self) -> ExecError {
        ExecError {
            code: self.code,
            message: self.message.clone(),
            detail: Some(self.detail.clone()),
            hint: None,
        }
    }
}

// ---------------------------------------------------------------- DOM

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Element(Elem),
    /// Decoded character data.
    Text(String),
    CData(String),
    Comment(String),
    /// (target, data)
    Pi(String, Option<String>),
    /// An entity reference that was declared external (or declared in
    /// an external DTD we do not load): PG/libxml keeps it unexpanded.
    EntityRef(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Elem {
    /// Written name, including any prefix.
    pub name: String,
    /// Resolved namespace URI of the element name (None = no namespace).
    pub ns: Option<String>,
    /// Real attributes in written order: (written name, value, ns uri).
    pub attrs: Vec<(String, String, Option<String>)>,
    /// xmlns declarations written on this element: (prefix, uri);
    /// prefix "" is the default namespace.
    pub ns_decls: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Decl {
    pub version: String,
    pub encoding: Option<String>,
    pub standalone: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Doc {
    pub decl: Option<Decl>,
    /// Raw DOCTYPE text (the full `<!DOCTYPE ...>` including brackets),
    /// preserved for verbatim round-trips of stored documents.
    pub doctype: Option<String>,
    /// Index into `nodes` before which the DOCTYPE appeared
    /// (nodes.len() at the moment it was parsed).
    pub doctype_pos: Option<usize>,
    pub nodes: Vec<Node>,
}

// ------------------------------------------------------------- parser

enum RefOut {
    Text(String),
    External(String),
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
    document: bool,
    /// Internal-subset entity declarations: name -> replacement text
    /// (None = external entity).
    entities: std::collections::HashMap<String, Option<String>>,
    /// True once a DOCTYPE was seen (undefined entities are then
    /// tolerated as external references, as libxml does when a DTD is
    /// present but not loaded).
    has_doctype: bool,
    /// PG_XML_STRICTNESS_ALL entry points (xpath, XMLTABLE) treat
    /// namespace problems (undeclared prefix, invalid URI) as
    /// errors; well-formedness-only entry points (xmlparse, casts)
    /// tolerate them like libxml warnings.
    strict_ns: bool,
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic()
        || c == '_'
        || c == ':'
        || (c as u32) > 0x7f && !matches!(c, '\u{00d7}' | '\u{00f7}')
}
fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || matches!(c, '-' | '.' | '\u{00b7}')
}

impl<'a> P<'a> {
    fn err(&self, detail: impl Into<String>) -> XmlErr {
        XmlErr::new(self.document, detail)
    }
    fn peek(&self) -> Option<char> {
        self.s().chars().next()
    }
    fn s(&self) -> &'a str {
        std::str::from_utf8(&self.b[self.i..]).unwrap_or("")
    }
    fn starts(&self, pat: &str) -> bool {
        self.s().starts_with(pat)
    }
    fn bump(&mut self, n: usize) {
        self.i += n;
    }
    fn eat(&mut self, pat: &str) -> bool {
        if self.starts(pat) {
            self.i += pat.len();
            true
        } else {
            false
        }
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, ' ' | '\t' | '\n' | '\r') {
                self.i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    fn parse_name(&mut self) -> Result<String, XmlErr> {
        let mut out = String::new();
        match self.peek() {
            Some(c) if is_name_start(c) => {
                out.push(c);
                self.i += c.len_utf8();
            }
            _ => return Err(self.err("expected an XML name")),
        }
        while let Some(c) = self.peek() {
            if is_name_char(c) {
                out.push(c);
                self.i += c.len_utf8();
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// Resolve one reference at `&`: predefined entities, character
    /// references, and internal-subset entities. Returns the decoded
    /// text (or an EntityRef marker via `Ok(None)` for external refs).
    fn parse_reference(&mut self) -> Result<RefOut, XmlErr> {
        // self.s() starts with '&'
        let rest = self.s();
        let semi = rest
            .find(';')
            .ok_or_else(|| self.err("unterminated entity reference"))?;
        let body = &rest[1..semi];
        if body.is_empty()
            || body.len() > 32
            || body
                .chars()
                .any(|c| matches!(c, '&' | '<' | ' ' | '\t' | '\n' | '\r'))
        {
            return Err(self.err("malformed entity reference"));
        }
        let decoded: RefOut =
            if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                let n = u32::from_str_radix(hex, 16)
                    .map_err(|_| self.err("invalid character reference"))?;
                RefOut::Text(valid_xml_char(n)?.to_string())
            } else if let Some(dec) = body.strip_prefix('#') {
                let n: u32 = dec
                    .parse()
                    .map_err(|_| self.err("invalid character reference"))?;
                RefOut::Text(valid_xml_char(n)?.to_string())
            } else {
                match body {
                    "amp" => RefOut::Text("&".into()),
                    "lt" => RefOut::Text("<".into()),
                    "gt" => RefOut::Text(">".into()),
                    "apos" => RefOut::Text("'".into()),
                    "quot" => RefOut::Text("\"".into()),
                    _ => match self.entities.get(body) {
                        Some(Some(repl)) => RefOut::Text(repl.clone()),
                        Some(None) => RefOut::External(body.to_string()),
                        None => {
                            if self.has_doctype {
                                // Declared (if anywhere) in an external
                                // DTD we never load: libxml tolerates it.
                                RefOut::External(body.to_string())
                            } else {
                                return Err(self.err(format!("undefined entity &{};", body)));
                            }
                        }
                    },
                }
            };
        self.bump(semi + 1);
        Ok(decoded)
    }

    /// Parse character data up to the next '<' or end of input,
    /// resolving references. Stops (without consuming) at '<'.
    /// Entity refs that stay unexpanded split the text run; they are
    /// returned as separate nodes by the caller loop — here we return
    /// the text accumulated before such a ref plus a flag.
    fn parse_text_run(&mut self) -> Result<(String, Option<String>), XmlErr> {
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Ok((out, None)),
                Some('<') => return Ok((out, None)),
                Some('&') => match self.parse_reference()? {
                    RefOut::Text(t) => out.push_str(&t),
                    RefOut::External(name) => return Ok((out, Some(name))),
                },
                Some(c) => {
                    if self.starts("]]>") {
                        return Err(self.err("']]>' is not allowed in character data"));
                    }
                    if c == '\r' {
                        // XML line-ending normalization.
                        self.bump(1);
                        if self.peek() == Some('\n') {
                            self.bump(1);
                        }
                        out.push('\n');
                    } else {
                        out.push(c);
                        self.i += c.len_utf8();
                    }
                }
            }
        }
    }

    fn parse_attr_value(&mut self, quote: char) -> Result<String, XmlErr> {
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated attribute value")),
                Some(c) if c == quote => {
                    self.bump(1);
                    return Ok(out);
                }
                Some('<') => {
                    return Err(self.err("'<' is not allowed in an attribute value"));
                }
                Some('&') => match self.parse_reference()? {
                    RefOut::Text(t) => out.push_str(&t),
                    RefOut::External(_) => {
                        return Err(self.err("external entity in attribute value"));
                    }
                },
                Some(c) => {
                    // libxml normalizes literal whitespace in
                    // attribute values to spaces.
                    if matches!(c, '\t' | '\n' | '\r') {
                        if c == '\r' && self.s()[1..].starts_with('\n') {
                            self.bump(1);
                        }
                        self.bump(c.len_utf8());
                        out.push(' ');
                    } else {
                        out.push(c);
                        self.i += c.len_utf8();
                    }
                }
            }
        }
    }

    fn parse_comment(&mut self) -> Result<Node, XmlErr> {
        // at "<!--"
        self.bump(4);
        let start = self.i;
        let rest = self.s();
        let end = rest
            .find("-->")
            .ok_or_else(|| self.err("unterminated comment"))?;
        let body = &rest[..end];
        if body.contains("--") || body.ends_with('-') {
            return Err(self.err("invalid comment content"));
        }
        self.i = start + end + 3;
        Ok(Node::Comment(body.to_string()))
    }

    fn parse_pi(&mut self) -> Result<Node, XmlErr> {
        // at "<?", target is not "xml" (handled by caller for decl).
        self.bump(2);
        let target = self.parse_name()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err(self.err("reserved processing instruction target"));
        }
        let data = if self.eat("?>") {
            None
        } else {
            // whitespace separates target from data
            match self.peek() {
                Some(c) if matches!(c, ' ' | '\t' | '\n' | '\r') => {}
                _ => {
                    return Err(self.err("expected whitespace after processing instruction target"));
                }
            }
            self.skip_ws();
            let rest = self.s();
            let end = rest
                .find("?>")
                .ok_or_else(|| self.err("unterminated processing instruction"))?;
            let d = &rest[..end];
            self.bump(end + 2);
            Some(d.to_string())
        };
        Ok(Node::Pi(target, data))
    }

    fn parse_cdata(&mut self) -> Result<Node, XmlErr> {
        // at "<![CDATA["
        self.bump(9);
        let rest = self.s();
        let end = rest
            .find("]]>")
            .ok_or_else(|| self.err("unterminated CDATA section"))?;
        let body = rest[..end].replace("\r\n", "\n").replace('\r', "\n");
        self.bump(end + 3);
        Ok(Node::CData(body))
    }

    /// Parse `<!DOCTYPE ...>` capturing the raw text; extract internal
    /// ENTITY declarations into self.entities.
    fn parse_doctype(&mut self) -> Result<String, XmlErr> {
        let start = self.i;
        self.bump(9); // <!DOCTYPE
        // scan with bracket/quote awareness
        let mut depth = 0i32;
        let mut quote: Option<char> = None;
        let mut j = self.i;
        let bytes = self.b;
        while j < bytes.len() {
            let c = bytes[j] as char;
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
            } else {
                match c {
                    '"' | '\'' => quote = Some(c),
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    '>' if depth <= 0 => {
                        let raw = std::str::from_utf8(&bytes[start..=j])
                            .unwrap_or("")
                            .to_string();
                        self.i = j + 1;
                        self.has_doctype = true;
                        self.extract_entities(&raw);
                        return Ok(raw);
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        Err(self.err("unterminated DOCTYPE declaration"))
    }

    fn extract_entities(&mut self, raw: &str) {
        // find internal subset
        let Some(lb) = raw.find('[') else { return };
        let Some(rb) = raw.rfind(']') else { return };
        if rb <= lb {
            return;
        }
        let subset = &raw[lb + 1..rb];
        let mut rest = subset;
        while let Some(pos) = rest.find("<!ENTITY") {
            rest = &rest[pos + 8..];
            let mut q = P {
                b: rest.as_bytes(),
                i: 0,
                document: self.document,
                entities: std::collections::HashMap::new(),
                has_doctype: true,
                strict_ns: self.strict_ns,
            };
            q.skip_ws();
            if q.starts("%") {
                continue; // parameter entities: not expanded in content
            }
            let Ok(name) = q.parse_name() else { continue };
            q.skip_ws();
            if q.starts("SYSTEM") || q.starts("PUBLIC") {
                self.entities.insert(name, None);
                continue;
            }
            match q.peek() {
                Some(c @ ('"' | '\'')) => {
                    q.bump(1);
                    // entity values: references to char/predefined are
                    // resolved; general refs stay literal (bypassed).
                    let mut val = String::new();
                    let mut ok = true;
                    loop {
                        match q.peek() {
                            None => {
                                ok = false;
                                break;
                            }
                            Some(cc) if cc == c => {
                                q.bump(1);
                                break;
                            }
                            Some('&') => match q.parse_reference() {
                                Ok(RefOut::Text(t)) => val.push_str(&t),
                                Ok(RefOut::External(n)) => {
                                    val.push('&');
                                    val.push_str(&n);
                                    val.push(';');
                                }
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            },
                            Some(cc) => {
                                val.push(cc);
                                q.bump(cc.len_utf8());
                            }
                        }
                    }
                    if ok {
                        self.entities.insert(name, Some(val));
                    }
                }
                _ => continue,
            }
        }
    }

    fn parse_decl(&mut self) -> Result<Decl, XmlErr> {
        // at "<?xml"
        self.bump(5);
        // version (required)
        self.skip_ws();
        let mut decl = Decl {
            version: String::new(),
            encoding: None,
            standalone: None,
        };
        let mut seen_version = false;
        loop {
            self.skip_ws();
            if self.eat("?>") {
                break;
            }
            let name = self.parse_name().map_err(|_| XmlErr {
                message: if self.document {
                    "invalid XML document".into()
                } else {
                    "invalid XML content".into()
                },
                detail: "invalid XML declaration".into(),
                code: if self.document { "2200M" } else { "2200N" },
            })?;
            self.skip_ws();
            if !self.eat("=") {
                return Err(self.decl_err());
            }
            self.skip_ws();
            let q = match self.peek() {
                Some(c @ ('"' | '\'')) => c,
                _ => return Err(self.decl_err()),
            };
            self.bump(1);
            let rest = self.s();
            let end = rest.find(q).ok_or_else(|| self.decl_err())?;
            let val = rest[..end].to_string();
            self.bump(end + 1);
            match name.as_str() {
                "version" => {
                    if val != "1.0" && val != "1.1" {
                        return Err(self.decl_err());
                    }
                    decl.version = val;
                    seen_version = true;
                }
                "encoding" => {
                    if val.is_empty()
                        || !val
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                    {
                        return Err(self.decl_err());
                    }
                    decl.encoding = Some(val);
                }
                "standalone" => {
                    decl.standalone = Some(match val.as_str() {
                        "yes" => true,
                        "no" => false,
                        _ => return Err(self.decl_err()),
                    });
                }
                _ => return Err(self.decl_err()),
            }
        }
        if !seen_version {
            return Err(self.decl_err());
        }
        Ok(decl)
    }

    fn decl_err(&self) -> XmlErr {
        XmlErr {
            message: if self.document {
                "invalid XML document".into()
            } else {
                "invalid XML content".into()
            },
            detail: "invalid XML declaration".into(),
            code: if self.document { "2200M" } else { "2200N" },
        }
    }

    /// Parse one element. `scopes` is the stack of in-scope namespace
    /// bindings (prefix -> uri), innermost last.
    fn parse_element(&mut self, scopes: &mut Vec<Vec<(String, String)>>) -> Result<Elem, XmlErr> {
        // at '<'
        self.bump(1);
        let name = self.parse_name()?;
        let mut raw_attrs: Vec<(String, String)> = Vec::new();
        let mut self_close = false;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('/') => {
                    self.bump(1);
                    if !self.eat(">") {
                        return Err(self.err("expected '>' after '/' in empty element tag"));
                    }
                    self_close = true;
                    break;
                }
                Some('>') => {
                    self.bump(1);
                    break;
                }
                Some(_) => {
                    let aname = self.parse_name()?;
                    if raw_attrs.iter().any(|(n, _)| *n == aname) {
                        return Err(self.err(format!("duplicate attribute \"{}\"", aname)));
                    }
                    self.skip_ws();
                    if !self.eat("=") {
                        return Err(self.err("expected '=' after attribute name"));
                    }
                    self.skip_ws();
                    let q = match self.peek() {
                        Some(c @ ('"' | '\'')) => c,
                        _ => return Err(self.err("expected quoted attribute value")),
                    };
                    self.bump(1);
                    let val = self.parse_attr_value(q)?;
                    raw_attrs.push((aname, val));
                }
                None => return Err(self.err("unterminated start tag")),
            }
        }
        // namespace declarations on this element
        let mut ns_decls: Vec<(String, String)> = Vec::new();
        for (n, v) in &raw_attrs {
            if n == "xmlns" {
                // Strict entry points (xpath/XMLTABLE) reject an
                // invalid namespace URI ('<'); a merely relative URI
                // is only a libxml warning in PG, not an error.
                if self.strict_ns && v.contains('<') {
                    return Err(self.err("invalid namespace URI".to_string()));
                }
                ns_decls.push((String::new(), v.clone()));
            } else if let Some(p) = n.strip_prefix("xmlns:") {
                if p == "xmlns" {
                    return Err(self.err("prefix \"xmlns\" must not be declared"));
                }
                if v.is_empty() {
                    return Err(self.err("empty namespace declaration"));
                }
                ns_decls.push((p.to_string(), v.clone()));
            }
        }
        scopes.push(ns_decls.clone());
        let resolve = |qname: &str,
                       scopes: &Vec<Vec<(String, String)>>,
                       p: &P|
         -> Result<Option<String>, XmlErr> {
            match qname.split_once(':') {
                Some((prefix, _local)) => {
                    if prefix == "xml" {
                        return Ok(Some("http://www.w3.org/XML/1998/namespace".to_string()));
                    }
                    if prefix == "xmlns" {
                        return Err(p.err("prefix \"xmlns\" is reserved"));
                    }
                    for scope in scopes.iter().rev() {
                        if let Some((_, uri)) = scope.iter().find(|(pf, _)| pf == prefix) {
                            return Ok(Some(uri.clone()));
                        }
                    }
                    if p.strict_ns {
                        return Err(
                            p.err(format!("namespace prefix \"{}\" is not defined", prefix))
                        );
                    }
                    // PG/libxml treats an undeclared prefix as a
                    // namespace warning, not a well-formedness error:
                    // the name is kept as-is with no namespace.
                    Ok(None)
                }
                None => {
                    for scope in scopes.iter().rev() {
                        if let Some((_, uri)) = scope.iter().find(|(pf, _)| pf.is_empty()) {
                            return Ok(if uri.is_empty() {
                                None
                            } else {
                                Some(uri.clone())
                            });
                        }
                    }
                    Ok(None)
                }
            }
        };
        let elem_ns = resolve(&name, scopes, self)?;
        let mut attrs: Vec<(String, String, Option<String>)> = Vec::new();
        for (n, v) in &raw_attrs {
            if n == "xmlns" || n.starts_with("xmlns:") {
                continue;
            }
            let ans = if n.contains(':') {
                resolve(n, scopes, self)?
            } else {
                None
            };
            // two attributes must not expand to the same (ns, local)
            let local = n.rsplit(':').next().unwrap_or(n);
            if attrs
                .iter()
                .any(|(on, _, ons)| ons == &ans && on.rsplit(':').next().unwrap_or(on) == local)
            {
                return Err(self.err("attributes with the same expanded name"));
            }
            attrs.push((n.clone(), v.clone(), ans));
        }
        let mut children: Vec<Node> = Vec::new();
        if !self_close {
            loop {
                if self.starts("</") {
                    self.bump(2);
                    let ename = self.parse_name()?;
                    self.skip_ws();
                    if !self.eat(">") {
                        return Err(self.err("expected '>' in end tag"));
                    }
                    if ename != name {
                        return Err(self.err(format!(
                            "end tag </{}> does not match start tag <{}>",
                            ename, name
                        )));
                    }
                    break;
                } else if self.starts("<!--") {
                    children.push(self.parse_comment()?);
                } else if self.starts("<![CDATA[") {
                    children.push(self.parse_cdata()?);
                } else if self.starts("<?") {
                    children.push(self.parse_pi()?);
                } else if self.starts("<!") {
                    return Err(self.err("markup declaration not allowed inside element content"));
                } else if self.starts("<") {
                    let el = self.parse_element(scopes)?;
                    children.push(Node::Element(el));
                } else if self.peek().is_none() {
                    return Err(self.err(format!("unterminated element <{}>", name)));
                } else if self.starts("&") {
                    // reference in content position
                    match self.parse_reference()? {
                        RefOut::Text(t) => {
                            if !t.is_empty() {
                                children.push(Node::Text(t));
                            }
                        }
                        RefOut::External(name) => {
                            children.push(Node::EntityRef(name));
                        }
                    }
                } else {
                    let (text, ext) = self.parse_text_run()?;
                    if !text.is_empty() {
                        children.push(Node::Text(text));
                    }
                    if let Some(name) = ext {
                        children.push(Node::EntityRef(name));
                    }
                }
            }
        }
        scopes.pop();
        Ok(Elem {
            name,
            ns: elem_ns,
            attrs,
            ns_decls,
            children,
        })
    }
}

fn valid_xml_char(n: u32) -> Result<char, XmlErr> {
    let c = char::from_u32(n).ok_or_else(|| XmlErr {
        message: "invalid XML content".into(),
        detail: "invalid character reference".into(),
        code: "2200N",
    })?;
    let v = n;
    if v == 0x9
        || v == 0xa
        || v == 0xd
        || (0x20..=0xd7ff).contains(&v)
        || (0xe000..=0xfffd).contains(&v)
        || (0x10000..=0x10ffff).contains(&v)
    {
        Ok(c)
    } else {
        Err(XmlErr {
            message: "invalid XML content".into(),
            detail: "invalid character reference".into(),
            code: "2200N",
        })
    }
}

// -------------------------------------------------- top-level parse

/// Parse `text` as XML. `document=true` enforces the document rules
/// (exactly one root element, only misc around it); `false` parses a
/// content fragment.
pub(crate) fn parse_xml(text: &str, document: bool) -> Result<Doc, XmlErr> {
    parse_xml_ns(text, document, false)
}

/// Parse with PG_XML_STRICTNESS_ALL namespace checks (xpath and
/// XMLTABLE document parsing in PG).
pub(crate) fn parse_xml_strict_ns(text: &str, document: bool) -> Result<Doc, XmlErr> {
    parse_xml_ns(text, document, true)
}

fn parse_xml_ns(text: &str, document: bool, strict_ns: bool) -> Result<Doc, XmlErr> {
    let mut p = P {
        b: text.as_bytes(),
        i: 0,
        document,
        entities: std::collections::HashMap::new(),
        has_doctype: false,
        strict_ns,
    };
    let mut doc = Doc {
        decl: None,
        doctype: None,
        doctype_pos: None,
        nodes: Vec::new(),
    };
    // XML declaration: only at the very start, "<?xml" + whitespace.
    if p.starts("<?xml") {
        let after = p.s()[5..].chars().next();
        if matches!(after, Some(c) if matches!(c, ' ' | '\t' | '\n' | '\r')) {
            doc.decl = Some(p.parse_decl()?);
        }
    }
    let mut scopes: Vec<Vec<(String, String)>> = Vec::new();
    let mut root_count = 0usize;
    loop {
        if p.peek().is_none() {
            break;
        }
        if p.starts("<!--") {
            let n = p.parse_comment()?;
            doc.nodes.push(n);
        } else if p.starts("<![CDATA[") {
            if document {
                return Err(p.err("CDATA section not allowed outside the root element"));
            }
            let n = p.parse_cdata()?;
            doc.nodes.push(n);
        } else if p.starts("<!DOCTYPE") {
            if document && root_count > 0 {
                return Err(p.err("DOCTYPE declaration after the root element"));
            }
            if doc.doctype.is_some() {
                return Err(p.err("duplicate DOCTYPE declaration"));
            }
            let raw = p.parse_doctype()?;
            doc.doctype_pos = Some(doc.nodes.len());
            doc.doctype = Some(raw);
        } else if p.starts("<?") {
            let n = p.parse_pi()?;
            doc.nodes.push(n);
        } else if p.starts("</") {
            return Err(p.err("unexpected end tag"));
        } else if p.starts("<") {
            let el = p.parse_element(&mut scopes)?;
            root_count += 1;
            if document && root_count > 1 {
                return Err(p.err("extra content after the root element"));
            }
            doc.nodes.push(Node::Element(el));
        } else if p.starts("&") {
            match p.parse_reference()? {
                RefOut::Text(t) => {
                    if document && !t.trim().is_empty() {
                        return Err(p.err("character data outside the root element"));
                    }
                    if !t.is_empty() {
                        doc.nodes.push(Node::Text(t));
                    }
                }
                RefOut::External(name) => {
                    if document {
                        return Err(p.err("entity reference outside the root element"));
                    }
                    doc.nodes.push(Node::EntityRef(name));
                }
            }
        } else {
            let (text, ext) = p.parse_text_run()?;
            if document && !text.trim().is_empty() {
                return Err(p.err("character data outside the root element"));
            }
            if !text.is_empty() {
                doc.nodes.push(Node::Text(text));
            }
            if let Some(name) = ext {
                if document {
                    return Err(p.err("entity reference outside the root element"));
                }
                doc.nodes.push(Node::EntityRef(name));
            }
        }
    }
    if document && root_count == 0 {
        return Err(p.err("no root element"));
    }
    Ok(doc)
}

// --------------------------------------------------------- serializer

pub(crate) fn escape_text_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
}

pub(crate) fn escape_attr_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
}

/// Escape a string as XML text content (libxml text-node dump
/// escapes & < > and renders CR as &#13;).
pub(crate) fn escape_text_content(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_text_into(s, &mut out);
    out
}

pub(crate) fn serialize_node(n: &Node, out: &mut String) {
    match n {
        Node::Element(e) => {
            out.push('<');
            out.push_str(&e.name);
            for (prefix, uri) in &e.ns_decls {
                if prefix.is_empty() {
                    out.push_str(" xmlns=\"");
                } else {
                    out.push_str(" xmlns:");
                    out.push_str(prefix);
                    out.push_str("=\"");
                }
                escape_attr_into(uri, out);
                out.push('"');
            }
            for (name, val, _) in &e.attrs {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape_attr_into(val, out);
                out.push('"');
            }
            if e.children.is_empty() {
                out.push_str("/>");
            } else {
                out.push('>');
                for c in &e.children {
                    serialize_node(c, out);
                }
                out.push_str("</");
                out.push_str(&e.name);
                out.push('>');
            }
        }
        Node::Text(t) => escape_text_into(t, out),
        Node::CData(t) => {
            out.push_str("<![CDATA[");
            out.push_str(t);
            out.push_str("]]>");
        }
        Node::Comment(t) => {
            out.push_str("<!--");
            out.push_str(t);
            out.push_str("-->");
        }
        Node::Pi(target, data) => {
            out.push_str("<?");
            out.push_str(target);
            if let Some(d) = data {
                if !d.is_empty() {
                    out.push(' ');
                    out.push_str(d);
                }
            }
            out.push_str("?>");
        }
        Node::EntityRef(name) => {
            out.push('&');
            out.push_str(name);
            out.push(';');
        }
    }
}

pub(crate) fn serialize_nodes(nodes: &[Node]) -> String {
    let mut out = String::new();
    for n in nodes {
        serialize_node(n, &mut out);
    }
    out
}

pub(crate) fn serialize_decl(d: &Decl) -> String {
    let mut out = String::from("<?xml version=\"");
    out.push_str(&d.version);
    out.push('"');
    if let Some(enc) = &d.encoding {
        out.push_str(" encoding=\"");
        out.push_str(enc);
        out.push('"');
    }
    if let Some(sa) = d.standalone {
        out.push_str(if sa {
            " standalone=\"yes\""
        } else {
            " standalone=\"no\""
        });
    }
    out.push_str("?>");
    out
}

pub(crate) fn serialize_doc(doc: &Doc) -> String {
    let mut out = String::new();
    if let Some(d) = &doc.decl {
        out.push_str(&serialize_decl(d));
    }
    if let Some(dt) = &doc.doctype {
        out.push_str(dt);
    }
    out.push_str(&serialize_nodes(&doc.nodes));
    out
}

// ---------------------------------------------------------- public API

/// The session's xmloption: true = DOCUMENT, false = CONTENT (the
/// PG19 default). Reads the per-statement GUC snapshot.
pub(crate) fn xmloption_is_document() -> bool {
    matches!(
        crate::exec::installed_guc("xmloption")
            .as_deref()
            .map(|v| v.trim().to_ascii_lowercase()),
        Some(ref v) if v == "document"
    )
}

/// Validate `text` as xml input per the session xmloption. On success
/// the text is stored verbatim (PG's xml storage form).
/// Validate a content-parsed value that contains a DOCTYPE against
/// document structure (PG xml_parse: content containing a DOCTYPE
/// is parsed as a document). Errors carry the CONTENT code, matching
/// xml_parse's use of the caller's xmloption for the report.
fn validate_content_as_document(doc: &Doc) -> Result<(), XmlErr> {
    let bad = || XmlErr {
        code: "2200N",
        message: "invalid XML content".to_string(),
        detail: String::new(),
    };
    let mut root_idx: Option<usize> = None;
    for (i, n) in doc.nodes.iter().enumerate() {
        match n {
            Node::Element(_) => {
                if root_idx.is_some() {
                    return Err(bad());
                }
                root_idx = Some(i);
            }
            Node::Text(t) if !t.trim().is_empty() => return Err(bad()),
            _ => {}
        }
    }
    let Some(root) = root_idx else {
        return Err(bad());
    };
    if let Some(pos) = doc.doctype_pos {
        if pos > root {
            return Err(bad());
        }
    }
    Ok(())
}

/// The text after a leading XML declaration (PG's parse_xml_decl
/// skip for CONTENT input). Assumes the declaration was already
/// validated by parse_xml.
fn strip_leading_decl(text: &str) -> &str {
    if !text.starts_with("<?xml") {
        return text;
    }
    let after = &text[5..];
    if !after.starts_with([' ', '\t', '\n', '\r']) {
        return text;
    }
    // Scan to the closing "?>" honoring quoted sections.
    let b = text.as_bytes();
    let mut i = 5;
    let mut quote: Option<u8> = None;
    while i + 1 < b.len() {
        match quote {
            Some(q) => {
                if b[i] == q {
                    quote = None;
                }
            }
            None => {
                if b[i] == b'"' || b[i] == b'\'' {
                    quote = Some(b[i]);
                } else if b[i] == b'?' && b[i + 1] == b'>' {
                    return &text[i + 2..];
                }
            }
        }
        i += 1;
    }
    text
}

pub(crate) fn xml_cast_in(text: &str) -> Result<String, ExecError> {
    if xmloption_is_document() {
        parse_xml(text, true).map_err(|e| e.to_exec())?;
        return Ok(text.to_string());
    }
    // CONTENT (PG xml_parse): validate; content containing a
    // DOCTYPE must satisfy document structure; the stored value is
    // the input with any XML declaration stripped.
    let doc = parse_xml(text, false).map_err(|e| e.to_exec())?;
    if doc.doctype.is_some() {
        validate_content_as_document(&doc).map_err(|e| e.to_exec())?;
    }
    Ok(strip_leading_decl(text).to_string())
}

/// PG `xml_is_document` semantics: parses as a document.
pub(crate) fn is_document(text: &str) -> bool {
    parse_xml(text, true).is_ok()
}

/// Full input-validation message for pg_input_error_info: PG reports
/// "invalid XML content: <detail>" style combined messages there.
pub(crate) fn xml_input_error_message(text: &str) -> Option<String> {
    let document = xmloption_is_document();
    match parse_xml(text, document) {
        Ok(_) => None,
        Err(e) => {
            if e.detail == "invalid XML declaration" {
                Some(format!("{}: {}", e.message, e.detail))
            } else {
                Some(e.message.clone())
            }
        }
    }
}

/// PG19 `map_sql_identifier_to_xml_name(ident, fully_escaped=false,
/// escape_period=false)` — the form used for XMLELEMENT/XMLFOREST/
/// XMLPI names.
pub(crate) fn map_sql_identifier_to_xml_name(ident: &str) -> String {
    map_xml_name(ident, false)
}

/// The fully-escaped form used by the table/query/schema mappings
/// (PG map_sql_identifier_to_xml_name with fully_escaped=true):
/// ':' is escaped everywhere, not just in first position.
pub(crate) fn map_sql_identifier_to_xml_name_fully(ident: &str) -> String {
    map_xml_name(ident, true)
}

fn map_xml_name(ident: &str, fully: bool) -> String {
    let mut out = String::new();
    let chars: Vec<char> = ident.chars().collect();
    for (idx, &c) in chars.iter().enumerate() {
        if c == ':' && (idx == 0 || fully) {
            out.push_str("_x003A_");
        } else if c == '_' && chars.get(idx + 1) == Some(&'x') {
            out.push_str("_x005F_");
        } else if idx == 0 && !is_name_start(c) {
            out.push_str(&format!("_x{:04X}_", c as u32));
        } else if idx > 0 && !is_name_char(c) {
            out.push_str(&format!("_x{:04X}_", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// PG19 `xmlroot`: rewrite the XML declaration of a document value.
/// `version`: None = not mentioned, Some(None) = NO VALUE,
/// Some(Some(v)) = set. Same shape for `standalone` (bool payload).
pub(crate) fn xml_root(
    text: &str,
    version: Option<Option<String>>,
    standalone: Option<Option<bool>>,
) -> Result<String, ExecError> {
    let mut doc = parse_xml(text, true).map_err(|e| e.to_exec())?;
    // PG xmlroot (xml.c): the result declaration carries the merged
    // fields — an option not mentioned keeps the document's value,
    // NO VALUE clears the field. A declaration is printed iff a
    // version or a standalone survives; a cleared version prints as
    // "1.0" when a declaration remains (libxml's default).
    let (mut ver, mut sa, enc) = match &doc.decl {
        Some(d) => (Some(d.version.clone()), d.standalone, d.encoding.clone()),
        None => (None, None, None),
    };
    if let Some(v) = &version {
        ver = v.clone();
    }
    if let Some(s) = &standalone {
        sa = *s;
    }
    doc.decl = if ver.is_some() || sa.is_some() {
        Some(Decl {
            version: ver.unwrap_or_else(|| "1.0".to_string()),
            encoding: enc,
            standalone: sa,
        })
    } else {
        None
    };
    Ok(serialize_doc(&doc))
}

/// The text content of a node (XPath string-value): for elements, the
/// concatenation of all descendant text/CDATA; for text-like nodes,
/// their content; comments/PIs per XPath rules.
pub(crate) fn string_value(n: &Node) -> String {
    match n {
        Node::Element(e) => {
            let mut out = String::new();
            for c in &e.children {
                out.push_str(&string_value(c));
            }
            out
        }
        Node::Text(t) | Node::CData(t) => t.clone(),
        Node::Comment(_) | Node::Pi(_, _) | Node::EntityRef(_) => String::new(),
    }
}

// ------------------------------------------------------------- XPath
//
// XPath 1.0 subset over the DOM, evaluated against node identity by
// index path (Vec<usize>) into the document forest; the empty path is
// the virtual document node whose children are the forest roots.
// Special node kinds produced by axes (attribute / namespace nodes)
// are carried as XNode values alongside path nodes.

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum XNode {
    /// Index path into the forest ([] = document node).
    Path(Vec<usize>),
    /// (owner path, attribute name, value, ns uri)
    Attr(Vec<usize>, String, String, Option<String>),
    /// (owner path, prefix, uri)
    Ns(Vec<usize>, String, String),
}

#[derive(Debug, Clone)]
pub(crate) enum XpValue {
    Nodes(Vec<XNode>),
    Str(String),
    Num(f64),
    Bool(bool),
}

pub(crate) struct XpErr(pub String);

fn xp_err<T>(msg: impl Into<String>) -> Result<T, XpErr> {
    Err(XpErr(msg.into()))
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Slash,
    DSlash,
    Dot,
    DotDot,
    At,
    ColonColon,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Pipe,
    Name(String),
    Str(String),
    Num(f64),
}

fn lex_xpath(s: &str) -> Result<Vec<Tok>, XpErr> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '/' => {
                if b.get(i + 1) == Some(&'/') {
                    out.push(Tok::DSlash);
                    i += 2;
                } else {
                    out.push(Tok::Slash);
                    i += 1;
                }
            }
            '.' => {
                if b.get(i + 1) == Some(&'.') {
                    out.push(Tok::DotDot);
                    i += 2;
                } else if b.get(i + 1).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    // .5 number
                    let mut j = i;
                    let mut txt = String::new();
                    while j < b.len() && (b[j].is_ascii_digit() || b[j] == '.') {
                        txt.push(b[j]);
                        j += 1;
                    }
                    out.push(Tok::Num(
                        txt.parse().map_err(|_| XpErr("bad number".into()))?,
                    ));
                    i = j;
                } else {
                    out.push(Tok::Dot);
                    i += 1;
                }
            }
            '@' => {
                out.push(Tok::At);
                i += 1;
            }
            ':' => {
                if b.get(i + 1) == Some(&':') {
                    out.push(Tok::ColonColon);
                    i += 2;
                } else {
                    return xp_err("unexpected ':' in XPath expression");
                }
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '=' => {
                out.push(Tok::Eq);
                i += 1;
            }
            '!' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Ne);
                    i += 2;
                } else {
                    return xp_err("unexpected '!' in XPath expression");
                }
            }
            '<' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Le);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            '+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            '*' => {
                out.push(Tok::Star);
                i += 1;
            }
            '|' => {
                out.push(Tok::Pipe);
                i += 1;
            }
            '\'' | '"' => {
                let q = c;
                let mut j = i + 1;
                let mut txt = String::new();
                loop {
                    match b.get(j) {
                        None => return xp_err("unterminated string literal in XPath"),
                        Some(&cc) if cc == q => break,
                        Some(&cc) => {
                            txt.push(cc);
                            j += 1;
                        }
                    }
                }
                out.push(Tok::Str(txt));
                i = j + 1;
            }
            _ if c.is_ascii_digit() => {
                let mut j = i;
                let mut txt = String::new();
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == '.') {
                    txt.push(b[j]);
                    j += 1;
                }
                out.push(Tok::Num(
                    txt.parse().map_err(|_| XpErr("bad number".into()))?,
                ));
                i = j;
            }
            _ if is_name_start(c) || c == '_' => {
                let mut j = i;
                let mut txt = String::new();
                while j < b.len() {
                    let cc = b[j];
                    if is_name_char(cc) && cc != ':' || cc == '_' {
                        txt.push(cc);
                        j += 1;
                    } else {
                        break;
                    }
                }
                // prefixed name: NCName ':' NCName (single colon)
                if b.get(j) == Some(&':') && b.get(j + 1) != Some(&':') {
                    let mut k = j + 1;
                    let mut local = String::new();
                    while k < b.len() && (is_name_char(b[k]) && b[k] != ':') {
                        local.push(b[k]);
                        k += 1;
                    }
                    if !local.is_empty() {
                        txt.push(':');
                        txt.push_str(&local);
                        j = k;
                    }
                }
                out.push(Tok::Name(txt));
                i = j;
            }
            _ => return xp_err(format!("unexpected character '{}' in XPath expression", c)),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub(crate) enum XExpr {
    Or(Box<XExpr>, Box<XExpr>),
    And(Box<XExpr>, Box<XExpr>),
    Cmp(Box<XExpr>, CmpOp, Box<XExpr>),
    Arith(Box<XExpr>, ArithOp, Box<XExpr>),
    Neg(Box<XExpr>),
    Union(Box<XExpr>, Box<XExpr>),
    Path {
        absolute: bool,
        steps: Vec<Step>,
    },
    /// `(expr)/step...` — apply steps to each node of a base result.
    StepFrom(Box<XExpr>, Vec<Step>),
    Filter(Box<XExpr>, Vec<XExpr>),
    Func(String, Vec<XExpr>),
    Str(String),
    Num(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone)]
pub(crate) struct Step {
    axis: Axis,
    test: NodeTest,
    preds: Vec<XExpr>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Axis {
    Child,
    Descendant,
    DescendantOrSelf,
    SelfAxis,
    Parent,
    Attribute,
    Namespace,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NodeTest {
    Name(String),
    AnyName,
    PrefixAny(String),
    Text,
    Node,
    Comment,
    Pi(Option<String>),
}

struct XParser {
    toks: Vec<Tok>,
    i: usize,
}

impl XParser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.i).cloned();
        if t.is_some() {
            self.i += 1;
        }
        t
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &Tok) -> Result<(), XpErr> {
        if self.eat(t) {
            Ok(())
        } else {
            xp_err("syntax error in XPath expression")
        }
    }
    fn peek_name(&self, want: &str) -> bool {
        matches!(self.peek(), Some(Tok::Name(n)) if n == want)
    }

    fn parse_expr(&mut self) -> Result<XExpr, XpErr> {
        self.parse_or()
    }
    fn parse_or(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_and()?;
        while self.peek_name("or") {
            self.next();
            let r = self.parse_and()?;
            l = XExpr::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn parse_and(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_eq()?;
        while self.peek_name("and") {
            self.next();
            let r = self.parse_eq()?;
            l = XExpr::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn parse_eq(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_rel()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => CmpOp::Eq,
                Some(Tok::Ne) => CmpOp::Ne,
                _ => break,
            };
            self.next();
            let r = self.parse_rel()?;
            l = XExpr::Cmp(Box::new(l), op, Box::new(r));
        }
        Ok(l)
    }
    fn parse_rel(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => CmpOp::Lt,
                Some(Tok::Le) => CmpOp::Le,
                Some(Tok::Gt) => CmpOp::Gt,
                Some(Tok::Ge) => CmpOp::Ge,
                _ => break,
            };
            self.next();
            let r = self.parse_add()?;
            l = XExpr::Cmp(Box::new(l), op, Box::new(r));
        }
        Ok(l)
    }
    fn parse_add(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => ArithOp::Add,
                Some(Tok::Minus) => ArithOp::Sub,
                _ => break,
            };
            self.next();
            let r = self.parse_mul()?;
            l = XExpr::Arith(Box::new(l), op, Box::new(r));
        }
        Ok(l)
    }
    fn parse_mul(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => ArithOp::Mul,
                Some(Tok::Name(n)) if n == "div" => ArithOp::Div,
                Some(Tok::Name(n)) if n == "mod" => ArithOp::Mod,
                _ => break,
            };
            self.next();
            let r = self.parse_unary()?;
            l = XExpr::Arith(Box::new(l), op, Box::new(r));
        }
        Ok(l)
    }
    fn parse_unary(&mut self) -> Result<XExpr, XpErr> {
        if self.eat(&Tok::Minus) {
            let e = self.parse_unary()?;
            return Ok(XExpr::Neg(Box::new(e)));
        }
        self.parse_union()
    }
    fn parse_union(&mut self) -> Result<XExpr, XpErr> {
        let mut l = self.parse_path_expr()?;
        while self.eat(&Tok::Pipe) {
            let r = self.parse_path_expr()?;
            l = XExpr::Union(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn parse_path_expr(&mut self) -> Result<XExpr, XpErr> {
        // location path starting with / or //
        if matches!(self.peek(), Some(Tok::Slash) | Some(Tok::DSlash)) {
            let dbl = self.eat(&Tok::DSlash);
            if !dbl {
                self.eat(&Tok::Slash);
            }
            let mut steps = Vec::new();
            if dbl {
                steps.push(Step {
                    axis: Axis::DescendantOrSelf,
                    test: NodeTest::Node,
                    preds: vec![],
                });
            }
            if !matches!(
                self.peek(),
                None | Some(Tok::RBracket)
                    | Some(Tok::RParen)
                    | Some(Tok::Comma)
                    | Some(Tok::Eq)
                    | Some(Tok::Ne)
                    | Some(Tok::Lt)
                    | Some(Tok::Le)
                    | Some(Tok::Gt)
                    | Some(Tok::Ge)
                    | Some(Tok::Plus)
                    | Some(Tok::Pipe)
            ) && !self.peek_name("and")
                && !self.peek_name("or")
            {
                self.parse_steps(&mut steps)?;
            }
            return Ok(XExpr::Path {
                absolute: true,
                steps,
            });
        }
        // primary or relative path
        let starts_step = matches!(
            self.peek(),
            Some(Tok::Dot) | Some(Tok::DotDot) | Some(Tok::At) | Some(Tok::Star)
        ) || matches!(self.peek(), Some(Tok::Name(_)))
            && !matches!(self.toks.get(self.i + 1), Some(Tok::LParen))
            || matches!(self.peek(), Some(Tok::Name(n)) if matches!(n.as_str(), "text" | "comment" | "node" | "processing-instruction"))
                && matches!(self.toks.get(self.i + 1), Some(Tok::LParen));
        if starts_step {
            let mut steps = Vec::new();
            self.parse_steps(&mut steps)?;
            return Ok(XExpr::Path {
                absolute: false,
                steps,
            });
        }
        // primary expression
        let mut e = self.parse_primary()?;
        // filter predicates
        let mut preds = Vec::new();
        while self.eat(&Tok::LBracket) {
            let p = self.parse_expr()?;
            self.expect(&Tok::RBracket)?;
            preds.push(p);
        }
        if !preds.is_empty() {
            e = XExpr::Filter(Box::new(e), preds);
        }
        // trailing steps after primary: (expr)/step
        if matches!(self.peek(), Some(Tok::Slash) | Some(Tok::DSlash)) {
            let mut steps = vec![Step {
                axis: Axis::SelfAxis,
                test: NodeTest::Node,
                preds: vec![],
            }];
            // convert: evaluate primary, then apply steps from it
            let dbl = self.peek() == Some(&Tok::DSlash);
            self.next();
            if dbl {
                steps.push(Step {
                    axis: Axis::DescendantOrSelf,
                    test: NodeTest::Node,
                    preds: vec![],
                });
            }
            self.parse_steps(&mut steps)?;
            return Ok(XExpr::StepFrom(Box::new(e), steps));
        }
        Ok(e)
    }

    fn parse_steps(&mut self, steps: &mut Vec<Step>) -> Result<(), XpErr> {
        loop {
            let step = self.parse_step()?;
            steps.push(step);
            if self.eat(&Tok::Slash) {
                continue;
            }
            if self.eat(&Tok::DSlash) {
                steps.push(Step {
                    axis: Axis::DescendantOrSelf,
                    test: NodeTest::Node,
                    preds: vec![],
                });
                continue;
            }
            break;
        }
        Ok(())
    }

    fn parse_step(&mut self) -> Result<Step, XpErr> {
        if self.eat(&Tok::Dot) {
            return Ok(Step {
                axis: Axis::SelfAxis,
                test: NodeTest::Node,
                preds: vec![],
            });
        }
        if self.eat(&Tok::DotDot) {
            return Ok(Step {
                axis: Axis::Parent,
                test: NodeTest::Node,
                preds: vec![],
            });
        }
        let mut axis = Axis::Child;
        if self.eat(&Tok::At) {
            axis = Axis::Attribute;
        } else if matches!(self.peek(), Some(Tok::Name(_)))
            && matches!(self.toks.get(self.i + 1), Some(Tok::ColonColon))
        {
            let aname = match self.next() {
                Some(Tok::Name(n)) => n,
                _ => unreachable!(),
            };
            self.next(); // ::
            axis = match aname.as_str() {
                "child" => Axis::Child,
                "descendant" => Axis::Descendant,
                "descendant-or-self" => Axis::DescendantOrSelf,
                "self" => Axis::SelfAxis,
                "parent" => Axis::Parent,
                "attribute" => Axis::Attribute,
                "namespace" => Axis::Namespace,
                _ => return xp_err(format!("unsupported axis \"{}\"", aname)),
            };
        }
        // node test
        let test = if self.eat(&Tok::Star) {
            NodeTest::AnyName
        } else {
            match self.next() {
                Some(Tok::Name(n)) => {
                    if self.peek() == Some(&Tok::LParen) {
                        // node-type test
                        self.next();
                        match n.as_str() {
                            "text" => {
                                self.expect(&Tok::RParen)?;
                                NodeTest::Text
                            }
                            "node" => {
                                self.expect(&Tok::RParen)?;
                                NodeTest::Node
                            }
                            "comment" => {
                                self.expect(&Tok::RParen)?;
                                NodeTest::Comment
                            }
                            "processing-instruction" => {
                                let arg = if self.peek() == Some(&Tok::RParen) {
                                    None
                                } else {
                                    match self.next() {
                                        Some(Tok::Str(s)) => Some(s),
                                        _ => return xp_err("bad processing-instruction test"),
                                    }
                                };
                                self.expect(&Tok::RParen)?;
                                NodeTest::Pi(arg)
                            }
                            _ => return xp_err(format!("unknown node test \"{}\"", n)),
                        }
                    } else if let Some(p) = n.strip_suffix(':') {
                        let _ = p;
                        NodeTest::PrefixAny(n.trim_end_matches(':').to_string())
                    } else if n.contains(':') && self.peek() == Some(&Tok::Star) {
                        self.next();
                        NodeTest::PrefixAny(n)
                    } else {
                        NodeTest::Name(n)
                    }
                }
                _ => return xp_err("expected a node test in XPath step"),
            }
        };
        // prefix:* arrives as Name("p:") handled above only when the
        // lexer glued it; also handle Name then Star.
        let test = if let NodeTest::Name(n) = &test {
            if n.ends_with(':') {
                NodeTest::PrefixAny(n.trim_end_matches(':').to_string())
            } else {
                test.clone()
            }
        } else {
            test
        };
        let mut preds = Vec::new();
        while self.eat(&Tok::LBracket) {
            let p = self.parse_expr()?;
            self.expect(&Tok::RBracket)?;
            preds.push(p);
        }
        Ok(Step { axis, test, preds })
    }

    fn parse_primary(&mut self) -> Result<XExpr, XpErr> {
        match self.next() {
            Some(Tok::LParen) => {
                let e = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Str(s)) => Ok(XExpr::Str(s)),
            Some(Tok::Num(n)) => Ok(XExpr::Num(n)),
            Some(Tok::Name(n)) => {
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    if !self.eat(&Tok::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.eat(&Tok::Comma) {
                                continue;
                            }
                            self.expect(&Tok::RParen)?;
                            break;
                        }
                    }
                    Ok(XExpr::Func(n, args))
                } else {
                    xp_err(format!("unexpected name \"{}\" in XPath expression", n))
                }
            }
            _ => xp_err("syntax error in XPath expression"),
        }
    }
}

// -------------------------------------------------------- XPath eval

pub(crate) struct Xp<'a> {
    pub forest: &'a [Node],
    /// Expression prefix -> namespace URI (xpath()'s ns array, plus
    /// XMLNAMESPACES for XMLTABLE).
    pub nsmap: &'a [(String, String)],
    /// XMLTABLE only: the XMLNAMESPACES DEFAULT uri — unprefixed
    /// expression names match elements in this namespace too
    /// (SQL/XML rules; plain XPath 1.0 leaves this None).
    pub default_ns: Option<&'a str>,
}

#[derive(Clone)]
struct Ctx {
    node: XNode,
    pos: usize,
    size: usize,
}

impl<'a> Xp<'a> {
    fn node_at(&self, path: &[usize]) -> Option<&'a Node> {
        let mut cur: &[Node] = self.forest;
        let mut node: Option<&Node> = None;
        for &ix in path {
            node = cur.get(ix);
            match node {
                Some(Node::Element(e)) => cur = &e.children,
                _ => return node,
            }
        }
        node
    }

    fn elem_at(&self, path: &[usize]) -> Option<&'a Elem> {
        match self.node_at(path) {
            Some(Node::Element(e)) => Some(e),
            _ => None,
        }
    }

    /// In-scope namespace bindings for the element at `path`
    /// (innermost declaration wins), excluding the implicit xml one.
    fn in_scope_ns(&self, path: &[usize]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        // walk from the root down to path, applying decls in order
        let mut prefixes: Vec<(String, String)> = Vec::new();
        for depth in 0..=path.len() {
            cur.truncate(depth);
            cur.extend_from_slice(&path[..depth]);
            if let Some(e) = self.elem_at(&cur) {
                for (p, u) in &e.ns_decls {
                    prefixes.retain(|(pp, _)| pp != p);
                    prefixes.push((p.clone(), u.clone()));
                }
            }
        }
        out.append(&mut prefixes);
        out
    }

    fn node_name(&self, n: &XNode) -> String {
        match n {
            XNode::Path(p) => {
                if p.is_empty() {
                    return String::new();
                }
                match self.node_at(p) {
                    Some(Node::Element(e)) => e.name.clone(),
                    Some(Node::Pi(t, _)) => t.clone(),
                    _ => String::new(),
                }
            }
            XNode::Attr(_, name, _, _) => name.clone(),
            XNode::Ns(_, prefix, _) => prefix.clone(),
        }
    }

    fn node_local(&self, n: &XNode) -> String {
        let name = self.node_name(n);
        match name.rsplit_once(':') {
            Some((_, l)) => l.to_string(),
            None => name,
        }
    }

    fn node_ns(&self, n: &XNode) -> Option<String> {
        match n {
            XNode::Path(p) => match self.node_at(p) {
                Some(Node::Element(e)) => e.ns.clone(),
                _ => None,
            },
            XNode::Attr(_, _, _, ns) => ns.clone(),
            XNode::Ns(_, _, uri) => Some(uri.clone()),
        }
    }

    pub(crate) fn node_string_value(&self, n: &XNode) -> String {
        match n {
            XNode::Path(p) => {
                if p.is_empty() {
                    // document node: string-value of the root element
                    let mut out = String::new();
                    for root in self.forest {
                        out.push_str(&string_value(root));
                    }
                    return out;
                }
                match self.node_at(p) {
                    Some(node) => string_value(node),
                    None => String::new(),
                }
            }
            XNode::Attr(_, _, v, _) => v.clone(),
            XNode::Ns(_, _, uri) => uri.clone(),
        }
    }

    /// Namespace declarations in scope at `path` (from ancestors,
    /// outermost first) that the subtree at `path` uses and does not
    /// itself redeclare — libxml's inherited-declarations behavior
    /// when dumping a node out of its document.
    fn inherited_ns_decls(&self, path: &[usize], target: &Elem) -> Vec<(String, String)> {
        // Namespace URIs used by the target subtree (elements and
        // attributes), plus the prefixes those uses are written with.
        fn collect_uses(e: &Elem, uses: &mut Vec<Option<String>>) {
            uses.push(e.ns.clone());
            for (_, _, ns) in &e.attrs {
                uses.push(ns.clone());
            }
            for c in &e.children {
                if let Node::Element(ce) = c {
                    collect_uses(ce, uses);
                }
            }
        }
        let mut uses: Vec<Option<String>> = Vec::new();
        collect_uses(target, &mut uses);
        // Walk ancestors from the forest root down to the parent.
        let mut scope: Vec<(String, String)> = Vec::new();
        let mut level: &[Node] = self.forest;
        for &idx in &path[..path.len().saturating_sub(1)] {
            let Some(Node::Element(parent)) = level.get(idx) else {
                return Vec::new();
            };
            for d in &parent.ns_decls {
                if let Some(slot) = scope.iter_mut().find(|(p, _)| *p == d.0) {
                    slot.1 = d.1.clone();
                } else {
                    scope.push(d.clone());
                }
            }
            level = &parent.children;
        }
        scope
            .into_iter()
            .filter(|(prefix, uri)| {
                !target.ns_decls.iter().any(|(p, _)| p == prefix)
                    && uses.iter().any(|u| u.as_deref() == Some(uri.as_str()))
            })
            .collect()
    }

    /// Serialize a result node the way PG's xpath() does: elements as
    /// markup, attribute nodes as their value, namespace nodes as the
    /// URI, text-like nodes as (escaped) text.
    pub(crate) fn node_result_text(&self, n: &XNode) -> String {
        match n {
            XNode::Path(p) => {
                if p.is_empty() {
                    return serialize_nodes(self.forest);
                }
                match self.node_at(p) {
                    Some(node) => {
                        // libxml dumps a result element with the
                        // namespace declarations inherited from its
                        // ancestors that its subtree actually uses.
                        if let Node::Element(e) = node {
                            let mut e2 = e.clone();
                            let inherited = self.inherited_ns_decls(p, e);
                            if !inherited.is_empty() {
                                let mut decls = inherited;
                                decls.extend(e2.ns_decls.iter().cloned());
                                e2.ns_decls = decls;
                            }
                            let mut out = String::new();
                            serialize_node(&Node::Element(e2), &mut out);
                            return out;
                        }
                        let mut out = String::new();
                        serialize_node(node, &mut out);
                        out
                    }
                    None => String::new(),
                }
            }
            // PG converts attribute results to an xml text node
            // holding the value: serialization escapes it.
            XNode::Attr(_, _, v, _) => escape_text_content(v),
            XNode::Ns(_, _, uri) => uri.clone(),
        }
    }

    fn children(&self, n: &XNode) -> Vec<XNode> {
        match n {
            XNode::Path(p) => {
                if p.is_empty() {
                    return self
                        .forest
                        .iter()
                        .enumerate()
                        .map(|(i, _)| XNode::Path(vec![i]))
                        .collect();
                }
                match self.node_at(p) {
                    Some(Node::Element(e)) => e
                        .children
                        .iter()
                        .enumerate()
                        .map(|(i, _)| {
                            let mut np = p.clone();
                            np.push(i);
                            XNode::Path(np)
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn descendants(&self, n: &XNode, out: &mut Vec<XNode>) {
        for c in self.children(n) {
            out.push(c.clone());
            self.descendants(&c, out);
        }
    }

    fn axis_nodes(&self, ctx: &XNode, axis: Axis) -> Vec<XNode> {
        match axis {
            Axis::Child => self.children(ctx),
            Axis::Descendant => {
                let mut out = Vec::new();
                self.descendants(ctx, &mut out);
                out
            }
            Axis::DescendantOrSelf => {
                let mut out = vec![ctx.clone()];
                self.descendants(ctx, &mut out);
                out
            }
            Axis::SelfAxis => vec![ctx.clone()],
            Axis::Parent => match ctx {
                XNode::Path(p) => {
                    if p.is_empty() {
                        Vec::new()
                    } else {
                        let mut parent = p.clone();
                        parent.pop();
                        vec![XNode::Path(parent)]
                    }
                }
                XNode::Attr(owner, ..) | XNode::Ns(owner, ..) => {
                    vec![XNode::Path(owner.clone())]
                }
            },
            Axis::Attribute => match ctx {
                XNode::Path(p) => match self.elem_at(p) {
                    Some(e) => e
                        .attrs
                        .iter()
                        .map(|(name, val, ns)| {
                            XNode::Attr(p.clone(), name.clone(), val.clone(), ns.clone())
                        })
                        .collect(),
                    None => Vec::new(),
                },
                _ => Vec::new(),
            },
            Axis::Namespace => match ctx {
                XNode::Path(p) => {
                    if self.elem_at(p).is_none() {
                        return Vec::new();
                    }
                    let mut out: Vec<XNode> = self
                        .in_scope_ns(p)
                        .into_iter()
                        .map(|(prefix, uri)| XNode::Ns(p.clone(), prefix, uri))
                        .collect();
                    out.push(XNode::Ns(
                        p.clone(),
                        "xml".into(),
                        "http://www.w3.org/XML/1998/namespace".into(),
                    ));
                    out
                }
                _ => Vec::new(),
            },
        }
    }

    fn test_matches(&self, n: &XNode, test: &NodeTest) -> Result<bool, XpErr> {
        match test {
            NodeTest::Node => Ok(true),
            NodeTest::Text => Ok(matches!(
                n,
                XNode::Path(p) if matches!(self.node_at(p), Some(Node::Text(_)) | Some(Node::CData(_)))
            )),
            NodeTest::Comment => Ok(matches!(
                n,
                XNode::Path(p) if matches!(self.node_at(p), Some(Node::Comment(_)))
            )),
            NodeTest::Pi(want) => match n {
                XNode::Path(p) => match self.node_at(p) {
                    Some(Node::Pi(t, _)) => Ok(want.as_deref().map(|w| w == t).unwrap_or(true)),
                    _ => Ok(false),
                },
                _ => Ok(false),
            },
            NodeTest::AnyName => match n {
                XNode::Path(p) => Ok(matches!(self.node_at(p), Some(Node::Element(_)))),
                XNode::Attr(..) => Ok(true),
                _ => Ok(false),
            },
            NodeTest::Name(name) => {
                let is_elem = matches!(n, XNode::Path(p) if matches!(self.node_at(p), Some(Node::Element(_))));
                if !is_elem && !matches!(n, XNode::Attr(..)) {
                    return Ok(false);
                }
                match name.split_once(':') {
                    Some((prefix, local)) => {
                        let uri = self
                            .nsmap
                            .iter()
                            .find(|(p, _)| p == prefix)
                            .map(|(_, u)| u.clone())
                            .or_else(|| {
                                if prefix == "xml" {
                                    Some("http://www.w3.org/XML/1998/namespace".to_string())
                                } else {
                                    None
                                }
                            });
                        let Some(uri) = uri else {
                            return xp_err(format!(
                                "namespace prefix \"{}\" is not defined",
                                prefix
                            ));
                        };
                        Ok(self.node_local(n) == local
                            && self.node_ns(n).as_deref() == Some(uri.as_str()))
                    }
                    None => Ok(self.node_name(n) == *name
                        && (self.node_ns(n).is_none()
                            || self.node_ns(n).as_deref() == self.default_ns)),
                }
            }
            NodeTest::PrefixAny(prefix) => {
                let uri = self
                    .nsmap
                    .iter()
                    .find(|(p, _)| p == prefix)
                    .map(|(_, u)| u.clone());
                let Some(uri) = uri else {
                    return xp_err(format!("namespace prefix \"{}\" is not defined", prefix));
                };
                let is_elem = matches!(n, XNode::Path(p) if matches!(self.node_at(p), Some(Node::Element(_))));
                Ok((is_elem || matches!(n, XNode::Attr(..)))
                    && self.node_ns(n).as_deref() == Some(uri.as_str()))
            }
        }
    }

    fn apply_step(&self, input: &[XNode], step: &Step) -> Result<Vec<XNode>, XpErr> {
        let mut out: Vec<XNode> = Vec::new();
        for ctx_node in input {
            let axis_nodes = self.axis_nodes(ctx_node, step.axis);
            let mut matched: Vec<XNode> = Vec::new();
            for n in axis_nodes {
                // attribute/namespace steps only make sense with their
                // own test kinds; name tests apply there too.
                let ok = match step.axis {
                    Axis::Attribute => {
                        matches!(n, XNode::Attr(..)) && self.test_matches(&n, &step.test)?
                    }
                    Axis::Namespace => {
                        matches!(n, XNode::Ns(..)) && self.ns_test_matches(&n, &step.test)
                    }
                    _ => self.test_matches(&n, &step.test)?,
                };
                if ok {
                    matched.push(n);
                }
            }
            // predicates
            for pred in &step.preds {
                let size = matched.len();
                let mut kept = Vec::new();
                for (idx, n) in matched.into_iter().enumerate() {
                    let c = Ctx {
                        node: n.clone(),
                        pos: idx + 1,
                        size,
                    };
                    let v = self.eval(pred, &c)?;
                    let keep = match &v {
                        XpValue::Num(x) => (idx + 1) as f64 == *x,
                        XpValue::Bool(b) => *b,
                        XpValue::Nodes(ns) => !ns.is_empty(),
                        XpValue::Str(s) => !s.is_empty(),
                    };
                    if keep {
                        kept.push(n);
                    }
                }
                matched = kept;
            }
            out.extend(matched);
        }
        Ok(dedup_sort_nodes(out))
    }

    fn ns_test_matches(&self, n: &XNode, test: &NodeTest) -> bool {
        match test {
            NodeTest::Node | NodeTest::AnyName => true,
            NodeTest::Name(name) => match n {
                XNode::Ns(_, prefix, _) => prefix == name,
                _ => false,
            },
            _ => false,
        }
    }

    pub(crate) fn eval_root(&self, e: &XExpr) -> Result<XpValue, XpErr> {
        let ctx = Ctx {
            node: XNode::Path(Vec::new()),
            pos: 1,
            size: 1,
        };
        self.eval(e, &ctx)
    }

    fn eval(&self, e: &XExpr, ctx: &Ctx) -> Result<XpValue, XpErr> {
        match e {
            XExpr::Str(s) => Ok(XpValue::Str(s.clone())),
            XExpr::Num(n) => Ok(XpValue::Num(*n)),
            XExpr::Path { absolute, steps } => {
                let start = if *absolute {
                    vec![XNode::Path(Vec::new())]
                } else {
                    vec![ctx.node.clone()]
                };
                let mut cur = start;
                for step in steps {
                    cur = self.apply_step(&cur, step)?;
                }
                Ok(XpValue::Nodes(cur))
            }
            XExpr::StepFrom(base, steps) => {
                let v = self.eval(base, ctx)?;
                let mut cur = match v {
                    XpValue::Nodes(ns) => ns,
                    _ => return xp_err("path base is not a node-set"),
                };
                for step in steps {
                    cur = self.apply_step(&cur, step)?;
                }
                Ok(XpValue::Nodes(cur))
            }
            XExpr::Filter(base, preds) => {
                let v = self.eval(base, ctx)?;
                let mut nodes = match v {
                    XpValue::Nodes(ns) => ns,
                    _ => return xp_err("predicate base is not a node-set"),
                };
                for pred in preds {
                    let size = nodes.len();
                    let mut kept = Vec::new();
                    for (idx, n) in nodes.into_iter().enumerate() {
                        let c = Ctx {
                            node: n.clone(),
                            pos: idx + 1,
                            size,
                        };
                        let pv = self.eval(pred, &c)?;
                        let keep = match &pv {
                            XpValue::Num(x) => (idx + 1) as f64 == *x,
                            XpValue::Bool(b) => *b,
                            XpValue::Nodes(ns) => !ns.is_empty(),
                            XpValue::Str(s) => !s.is_empty(),
                        };
                        if keep {
                            kept.push(n);
                        }
                    }
                    nodes = kept;
                }
                Ok(XpValue::Nodes(nodes))
            }
            XExpr::Or(l, r) => {
                let lb = self.to_bool(&self.eval(l, ctx)?);
                if lb {
                    return Ok(XpValue::Bool(true));
                }
                Ok(XpValue::Bool(self.to_bool(&self.eval(r, ctx)?)))
            }
            XExpr::And(l, r) => {
                let lb = self.to_bool(&self.eval(l, ctx)?);
                if !lb {
                    return Ok(XpValue::Bool(false));
                }
                Ok(XpValue::Bool(self.to_bool(&self.eval(r, ctx)?)))
            }
            XExpr::Cmp(l, op, r) => {
                let lv = self.eval(l, ctx)?;
                let rv = self.eval(r, ctx)?;
                Ok(XpValue::Bool(self.compare(&lv, *op, &rv)))
            }
            XExpr::Arith(l, op, r) => {
                let a = self.to_number(&self.eval(l, ctx)?);
                let b = self.to_number(&self.eval(r, ctx)?);
                let v = match op {
                    ArithOp::Add => a + b,
                    ArithOp::Sub => a - b,
                    ArithOp::Mul => a * b,
                    ArithOp::Div => a / b,
                    ArithOp::Mod => a % b,
                };
                Ok(XpValue::Num(v))
            }
            XExpr::Neg(x) => Ok(XpValue::Num(-self.to_number(&self.eval(x, ctx)?))),
            XExpr::Union(l, r) => {
                let mut a = match self.eval(l, ctx)? {
                    XpValue::Nodes(ns) => ns,
                    _ => return xp_err("union operands must be node-sets"),
                };
                let b = match self.eval(r, ctx)? {
                    XpValue::Nodes(ns) => ns,
                    _ => return xp_err("union operands must be node-sets"),
                };
                a.extend(b);
                Ok(XpValue::Nodes(dedup_sort_nodes(a)))
            }
            XExpr::Func(name, args) => self.eval_func(name, args, ctx),
        }
    }

    fn eval_func(&self, name: &str, args: &[XExpr], ctx: &Ctx) -> Result<XpValue, XpErr> {
        let arg = |i: usize, xp: &Self| -> Result<XpValue, XpErr> {
            match args.get(i) {
                Some(e) => xp.eval(e, ctx),
                None => Ok(XpValue::Nodes(vec![ctx.node.clone()])),
            }
        };
        match name {
            "count" => {
                let v = arg(0, self)?;
                match v {
                    XpValue::Nodes(ns) => Ok(XpValue::Num(ns.len() as f64)),
                    _ => xp_err("count() requires a node-set"),
                }
            }
            "name" => {
                let v = arg(0, self)?;
                match v {
                    XpValue::Nodes(ns) => Ok(XpValue::Str(
                        ns.first().map(|n| self.node_name(n)).unwrap_or_default(),
                    )),
                    _ => xp_err("name() requires a node-set"),
                }
            }
            "local-name" => {
                let v = arg(0, self)?;
                match v {
                    XpValue::Nodes(ns) => Ok(XpValue::Str(
                        ns.first().map(|n| self.node_local(n)).unwrap_or_default(),
                    )),
                    _ => xp_err("local-name() requires a node-set"),
                }
            }
            "namespace-uri" => {
                let v = arg(0, self)?;
                match v {
                    XpValue::Nodes(ns) => Ok(XpValue::Str(
                        ns.first().and_then(|n| self.node_ns(n)).unwrap_or_default(),
                    )),
                    _ => xp_err("namespace-uri() requires a node-set"),
                }
            }
            "string" => {
                let v = arg(0, self)?;
                Ok(XpValue::Str(self.to_string_val(&v)))
            }
            "string-length" => {
                let v = arg(0, self)?;
                let s = self.to_string_val(&v);
                Ok(XpValue::Num(s.chars().count() as f64))
            }
            "concat" => {
                let mut out = String::new();
                for a in args {
                    let v = self.eval(a, ctx)?;
                    out.push_str(&self.to_string_val(&v));
                }
                Ok(XpValue::Str(out))
            }
            "contains" => {
                let a = self.to_string_val(&arg(0, self)?);
                let b = self.to_string_val(&arg(1, self)?);
                Ok(XpValue::Bool(a.contains(&b)))
            }
            "starts-with" => {
                let a = self.to_string_val(&arg(0, self)?);
                let b = self.to_string_val(&arg(1, self)?);
                Ok(XpValue::Bool(a.starts_with(&b)))
            }
            "substring-before" => {
                let a = self.to_string_val(&arg(0, self)?);
                let b = self.to_string_val(&arg(1, self)?);
                Ok(XpValue::Str(
                    a.split_once(&b)
                        .map(|(x, _)| x.to_string())
                        .unwrap_or_default(),
                ))
            }
            "substring-after" => {
                let a = self.to_string_val(&arg(0, self)?);
                let b = self.to_string_val(&arg(1, self)?);
                Ok(XpValue::Str(
                    a.split_once(&b)
                        .map(|(_, x)| x.to_string())
                        .unwrap_or_default(),
                ))
            }
            "normalize-space" => {
                let v = arg(0, self)?;
                let s = self.to_string_val(&v);
                Ok(XpValue::Str(
                    s.split_whitespace().collect::<Vec<_>>().join(" "),
                ))
            }
            "not" => {
                let v = arg(0, self)?;
                Ok(XpValue::Bool(!self.to_bool(&v)))
            }
            "boolean" => {
                let v = arg(0, self)?;
                Ok(XpValue::Bool(self.to_bool(&v)))
            }
            "number" => {
                let v = arg(0, self)?;
                Ok(XpValue::Num(self.to_number(&v)))
            }
            "sum" => {
                let v = arg(0, self)?;
                match v {
                    XpValue::Nodes(ns) => {
                        let mut total = 0.0;
                        for n in &ns {
                            total += xpath_number(&self.node_string_value(n));
                        }
                        Ok(XpValue::Num(total))
                    }
                    _ => xp_err("sum() requires a node-set"),
                }
            }
            "floor" => Ok(XpValue::Num(self.to_number(&arg(0, self)?).floor())),
            "ceiling" => Ok(XpValue::Num(self.to_number(&arg(0, self)?).ceil())),
            "round" => Ok(XpValue::Num(self.to_number(&arg(0, self)?).round())),
            "position" => Ok(XpValue::Num(ctx.pos as f64)),
            "last" => Ok(XpValue::Num(ctx.size as f64)),
            "true" => Ok(XpValue::Bool(true)),
            "false" => Ok(XpValue::Bool(false)),
            _ => xp_err(format!("function \"{}\" is not available in XPath", name)),
        }
    }

    pub(crate) fn to_bool(&self, v: &XpValue) -> bool {
        match v {
            XpValue::Bool(b) => *b,
            XpValue::Nodes(ns) => !ns.is_empty(),
            XpValue::Str(s) => !s.is_empty(),
            XpValue::Num(n) => *n != 0.0 && !n.is_nan(),
        }
    }

    pub(crate) fn to_number(&self, v: &XpValue) -> f64 {
        match v {
            XpValue::Num(n) => *n,
            XpValue::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            XpValue::Str(s) => xpath_number(s),
            XpValue::Nodes(ns) => ns
                .first()
                .map(|n| xpath_number(&self.node_string_value(n)))
                .unwrap_or(f64::NAN),
        }
    }

    pub(crate) fn to_string_val(&self, v: &XpValue) -> String {
        match v {
            XpValue::Str(s) => s.clone(),
            XpValue::Num(n) => xpath_number_text(*n),
            XpValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            XpValue::Nodes(ns) => ns
                .first()
                .map(|n| self.node_string_value(n))
                .unwrap_or_default(),
        }
    }

    fn compare(&self, l: &XpValue, op: CmpOp, r: &XpValue) -> bool {
        use CmpOp::*;
        // node-set comparisons: existential over pairs
        if let (XpValue::Nodes(a), XpValue::Nodes(b)) = (l, r) {
            return match op {
                Eq | Ne => {
                    let mut any = false;
                    for x in a {
                        for y in b {
                            let eq = self.node_string_value(x) == self.node_string_value(y);
                            if (op == Eq) == eq {
                                any = true;
                            }
                        }
                    }
                    any
                }
                _ => {
                    let mut any = false;
                    for x in a {
                        for y in b {
                            if num_cmp(
                                xpath_number(&self.node_string_value(x)),
                                op,
                                xpath_number(&self.node_string_value(y)),
                            ) {
                                any = true;
                            }
                        }
                    }
                    any
                }
            };
        }
        if let XpValue::Nodes(a) = l {
            return self.compare_nodes_scalar(a, op, r, true);
        }
        if let XpValue::Nodes(b) = r {
            return self.compare_nodes_scalar(b, flip_op(op), l, true);
        }
        match op {
            Eq | Ne => {
                let eq = if matches!(l, XpValue::Bool(_)) || matches!(r, XpValue::Bool(_)) {
                    self.to_bool(l) == self.to_bool(r)
                } else if matches!(l, XpValue::Num(_)) || matches!(r, XpValue::Num(_)) {
                    self.to_number(l) == self.to_number(r)
                } else {
                    self.to_string_val(l) == self.to_string_val(r)
                };
                if op == Eq { eq } else { !eq }
            }
            _ => num_cmp(self.to_number(l), op, self.to_number(r)),
        }
    }

    fn compare_nodes_scalar(
        &self,
        nodes: &[XNode],
        op: CmpOp,
        scalar: &XpValue,
        _nodes_left: bool,
    ) -> bool {
        use CmpOp::*;
        match scalar {
            XpValue::Bool(b) => {
                let nb = !nodes.is_empty();
                match op {
                    Eq => nb == *b,
                    Ne => nb != *b,
                    _ => false,
                }
            }
            XpValue::Num(n) => {
                let mut any = false;
                for x in nodes {
                    if num_cmp(xpath_number(&self.node_string_value(x)), op, *n) {
                        any = true;
                    }
                }
                any
            }
            XpValue::Str(s) => {
                let mut any_eq = false;
                let mut any_ne = false;
                for x in nodes {
                    let sv = self.node_string_value(x);
                    if sv == *s {
                        any_eq = true;
                    } else {
                        any_ne = true;
                    }
                }
                match op {
                    Eq => any_eq,
                    Ne => any_ne,
                    _ => {
                        let mut any = false;
                        for x in nodes {
                            if num_cmp(
                                xpath_number(&self.node_string_value(x)),
                                op,
                                xpath_number(s),
                            ) {
                                any = true;
                            }
                        }
                        any
                    }
                }
            }
            XpValue::Nodes(_) => unreachable!(),
        }
    }
}

fn flip_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

fn num_cmp(a: f64, op: CmpOp, b: f64) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

/// XPath number() conversion: trimmed, optional '-', digits with at
/// most one '.', at least one digit; anything else is NaN.
pub(crate) fn xpath_number(s: &str) -> f64 {
    let t = s.trim();
    let body = t.strip_prefix('-').unwrap_or(t);
    if body.is_empty()
        || !body.chars().all(|c| c.is_ascii_digit() || c == '.')
        || body.matches('.').count() > 1
        || !body.chars().any(|c| c.is_ascii_digit())
    {
        return f64::NAN;
    }
    t.parse().unwrap_or(f64::NAN)
}

/// XPath string rendering of a number (also PG's xpath() rendering).
pub(crate) fn xpath_number_text(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == n.trunc() && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    let s = format!("{}", n);
    s
}

fn node_sort_key(n: &XNode) -> (Vec<usize>, u8, String) {
    match n {
        XNode::Path(p) => (p.clone(), 0, String::new()),
        XNode::Attr(owner, name, _, _) => (owner.clone(), 1, name.clone()),
        XNode::Ns(owner, prefix, _) => (owner.clone(), 2, prefix.clone()),
    }
}

fn dedup_sort_nodes(mut nodes: Vec<XNode>) -> Vec<XNode> {
    nodes.sort_by(|a, b| node_sort_key(a).cmp(&node_sort_key(b)));
    nodes.dedup();
    nodes
}

/// Compile an XPath expression. PG errors: empty -> "empty XPath
/// expression"; malformed -> "invalid XPath expression" (both 10608).
pub(crate) fn compile_xpath(expr: &str) -> Result<XExpr, ExecError> {
    if expr.is_empty() {
        return Err(ExecError {
            code: "10608",
            message: "empty XPath expression".to_string(),
            detail: None,
            hint: None,
        });
    }
    let toks = lex_xpath(expr).map_err(|_| ExecError {
        code: "10608",
        message: "invalid XPath expression".to_string(),
        detail: None,
        hint: None,
    })?;
    let mut p = XParser { toks, i: 0 };
    let e = p.parse_expr().map_err(|_| ExecError {
        code: "10608",
        message: "invalid XPath expression".to_string(),
        detail: None,
        hint: None,
    })?;
    if p.i != p.toks.len() {
        return Err(ExecError {
            code: "10608",
            message: "invalid XPath expression".to_string(),
            detail: None,
            hint: None,
        });
    }
    Ok(e)
}

/// Evaluate a compiled XPath over a parsed document with the document
/// node as the initial context (PG xpath() semantics).
pub(crate) fn eval_xpath(
    doc: &Doc,
    expr: &XExpr,
    nsmap: &[(String, String)],
) -> Result<XpValue, ExecError> {
    let xp = Xp {
        forest: &doc.nodes,
        nsmap,
        default_ns: None,
    };
    xp.eval_root(expr).map_err(|e| ExecError {
        code: "10608",
        message: "invalid XPath expression".to_string(),
        detail: Some(e.0),
        hint: None,
    })
}

/// eval_xpath_at with an XMLTABLE XMLNAMESPACES DEFAULT uri.
pub(crate) fn eval_xpath_at_ns(
    doc: &Doc,
    expr: &XExpr,
    nsmap: &[(String, String)],
    ctx_path: &[usize],
    default_ns: Option<&str>,
) -> Result<XpValue, ExecError> {
    let xp = Xp {
        forest: &doc.nodes,
        nsmap,
        default_ns,
    };
    let ctx = Ctx {
        node: XNode::Path(ctx_path.to_vec()),
        pos: 1,
        size: 1,
    };
    xp.eval(expr, &ctx).map_err(|e| ExecError {
        code: "10608",
        message: "invalid XPath expression".to_string(),
        detail: Some(e.0),
        hint: None,
    })
}

/// PG xpath() result conversion: node-sets serialize node by node;
/// scalar results become a single text item.
pub(crate) fn xpath_result_strings(
    doc: &Doc,
    v: &XpValue,
    nsmap: &[(String, String)],
) -> Vec<String> {
    let xp = Xp {
        forest: &doc.nodes,
        nsmap,
        default_ns: None,
    };
    match v {
        XpValue::Nodes(ns) => ns.iter().map(|n| xp.node_result_text(n)).collect(),
        XpValue::Str(s) => vec![escape_text_content(s)],
        XpValue::Num(n) => vec![xpath_number_text(*n)],
        XpValue::Bool(b) => vec![if *b { "true" } else { "false" }.to_string()],
    }
}

// -------------------------------------------------- SQL/XML functions

fn err(code: &'static str, message: impl Into<String>) -> ExecError {
    ExecError {
        code,
        message: message.into(),
        detail: None,
        hint: None,
    }
}

/// PG `xmlcomment` (2200S on invalid content).
pub(crate) fn xml_comment(text: &str) -> Result<Value, ExecError> {
    if text.contains("--") || text.ends_with('-') {
        return Err(err("2200S", "invalid XML comment"));
    }
    Ok(Value::xml(format!("<!--{}-->", text)))
}

/// libxml xmlEncodeSpecialChars (xmltext's escaping): & < > " and CR.
pub(crate) fn xml_encode_special(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

/// PG `xmltext`: the text as an xml value with content escaped.
pub(crate) fn xml_text(text: &str) -> Value {
    Value::xml(xml_encode_special(text))
}

/// Strip one leading XML declaration from `text`, returning
/// (rest, version, standalone) where standalone is -1 absent /
/// 0 "no" / 1 "yes" (xmlconcat's parse_xml_decl model).
fn strip_decl(text: &str) -> (&str, Option<String>, i32) {
    if !text.starts_with("<?xml") {
        return (text, None, -1);
    }
    let after = &text[5..];
    if !after.starts_with([' ', '\t', '\n', '\r']) {
        return (text, None, -1);
    }
    let Some(end) = after.find("?>") else {
        return (text, None, -1);
    };
    let decl = &after[..end];
    let mut version = None;
    let mut standalone = -1;
    for part in decl.split_whitespace() {
        if let Some(v) = part.strip_prefix("version=") {
            version = Some(v.trim_matches(|c| c == '"' || c == '\'').to_string());
        } else if let Some(v) = part.strip_prefix("standalone=") {
            standalone = match v.trim_matches(|c| c == '"' || c == '\'') {
                "yes" => 1,
                "no" => 0,
                _ => -1,
            };
        }
    }
    (&text[5 + end + 2..], version, standalone)
}

/// PG `xmlconcat` (xml.c): text-level concatenation with declaration
/// merging — no content parsing.
pub(crate) fn xml_concat(texts: &[String]) -> Value {
    let mut global_version: Option<String> = None;
    let mut version_no_value = false;
    let mut global_standalone: i32 = 1;
    let mut buf = String::new();
    for t in texts {
        let (rest, version, standalone) = strip_decl(t);
        if standalone == 0 && global_standalone == 1 {
            global_standalone = 0;
        }
        if standalone < 0 {
            global_standalone = -1;
        }
        match version {
            None => version_no_value = true,
            Some(v) => match &global_version {
                None => global_version = Some(v),
                Some(gv) if *gv != v => version_no_value = true,
                _ => {}
            },
        }
        buf.push_str(rest);
    }
    if !version_no_value || global_standalone >= 0 {
        let mut out = String::from("<?xml version=\"");
        out.push_str(global_version.as_deref().unwrap_or("1.0"));
        out.push('"');
        if global_standalone >= 0 {
            out.push_str(if global_standalone == 1 {
                " standalone=\"yes\""
            } else {
                " standalone=\"no\""
            });
        }
        out.push_str("?>");
        out.push_str(&buf);
        Value::xml(out)
    } else {
        Value::xml(buf)
    }
}

/// The text form of a value for XML content/attribute mapping
/// (PG map_sql_value_to_xml_value): bytea honors xmlbinary; booleans
/// render true/false; xml values contribute their stored text for
/// attributes and their parsed nodes for content.
pub(crate) fn xml_binary_is_hex() -> bool {
    matches!(
        crate::exec::installed_guc("xmlbinary")
            .as_deref()
            .map(|v| v.trim().to_ascii_lowercase()),
        Some(ref v) if v == "hex"
    )
}

pub(crate) fn bytea_xml_text(bytes: &[u8]) -> String {
    if xml_binary_is_hex() {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{:02x}", b));
        }
        out
    } else {
        base64_encode(bytes)
    }
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TBL[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TBL[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Scalar value -> XML content text (unescaped; callers escape or
/// wrap). None for NULL. Timestamps use the XML Schema lexical form
/// (PG map_sql_value_to_xml_value via datatype_to_xml): ISO with a
/// 'T' separator; infinities are an error.
pub(crate) fn scalar_xml_text(v: &Value) -> Result<Option<String>, ExecError> {
    Ok(match v {
        Value::Null => None,
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Bytea(bytes) => Some(bytea_xml_text(bytes)),
        Value::Xml(t) => Some(t.to_string()),
        Value::Timestamp(micros) => {
            if *micros == crate::datetime::TS_INFINITY
                || *micros == crate::datetime::TS_NEG_INFINITY
            {
                return Err(err("2200M", "could not convert timestamp to XML: infinity"));
            }
            // XML Schema lexical form: ISO with a 'T' separator,
            // independent of DateStyle (PG datatype_to_xml).
            Some(crate::datetime::format_timestamp_iso(*micros).replacen(' ', "T", 1))
        }
        Value::Timestamptz(micros) => {
            if *micros == crate::datetime::TS_INFINITY
                || *micros == crate::datetime::TS_NEG_INFINITY
            {
                return Err(err("2200M", "could not convert timestamp to XML: infinity"));
            }
            Some(crate::datetime::format_timestamptz_iso(*micros).replacen(' ', "T", 1))
        }
        Value::Date(d) => {
            if *d == crate::datetime::DATE_INFINITY || *d == crate::datetime::DATE_NEG_INFINITY {
                return Err(err("2200M", "could not convert date to XML: infinity"));
            }
            // XML Schema lexical form (ISO), independent of DateStyle
            // (PG map_sql_value_to_xml_value uses USE_XSD_DATES).
            Some(crate::datetime::format_date_iso(*d))
        }
        other => other.to_text(),
    })
}

/// A value as XMLELEMENT/XMLFOREST content nodes (PG
/// map_sql_value_to_xml_value + print_xml_value): xml values splice
/// their parsed content; arrays become <element> children; other
/// scalars become text.
pub(crate) fn content_nodes(v: &Value) -> Result<Vec<Node>, ExecError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Xml(t) => {
            let doc = parse_xml(t, false).map_err(|e| e.to_exec())?;
            Ok(doc.nodes)
        }
        Value::Array(arr) => {
            let mut out = Vec::new();
            for e in &arr.elems {
                out.push(Node::Element(Elem {
                    name: "element".to_string(),
                    ns: None,
                    attrs: Vec::new(),
                    ns_decls: Vec::new(),
                    children: content_nodes(e)?,
                }));
            }
            Ok(out)
        }
        other => Ok(vec![Node::Text(
            scalar_xml_text(other)?.unwrap_or_default(),
        )]),
    }
}

/// A value as an XML attribute value (unescaped text). None = omit
/// the attribute (NULL). xml values contribute their markup text.
pub(crate) fn attr_text(v: &Value) -> Result<Option<String>, ExecError> {
    scalar_xml_text(v)
}

/// Recursively drop whitespace-only text nodes that sit among
/// element siblings (XMLPARSE STRIP WHITESPACE / libxml
/// keep-blanks-off semantics).
pub(crate) fn strip_ws_nodes(nodes: &mut Vec<Node>) {
    strip_ws_for_indent(nodes);
}

/// xmlserialize INDENT formatting (libxml format-output model):
/// elements whose children are only elements/comments/PIs (plus
/// ignorable whitespace) are laid out one child per line with
/// two-space indent; mixed-content subtrees serialize verbatim.
fn format_node(n: &Node, depth: usize, out: &mut String) {
    match n {
        Node::Element(e) => {
            let element_only = !e.children.is_empty()
                && e.children
                    .iter()
                    .all(|c| matches!(c, Node::Element(_) | Node::Comment(_) | Node::Pi(_, _)));
            if !element_only {
                serialize_node(n, out);
                return;
            }
            out.push('<');
            out.push_str(&e.name);
            write_elem_head(e, out);
            out.push('>');
            for c in &e.children {
                out.push('\n');
                out.push_str(&"  ".repeat(depth + 1));
                format_node(c, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push_str("</");
            out.push_str(&e.name);
            out.push('>');
        }
        _ => serialize_node(n, out),
    }
}

fn write_elem_head(e: &Elem, out: &mut String) {
    for (prefix, uri) in &e.ns_decls {
        if prefix.is_empty() {
            out.push_str(" xmlns=\"");
        } else {
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
        }
        escape_attr_into(uri, out);
        out.push('"');
    }
    for (name, val, _) in &e.attrs {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        escape_attr_into(val, out);
        out.push('"');
    }
}

/// Format a parsed value for xmlserialize(INDENT). `document` selects
/// declaration/DOCTYPE handling: documents keep their declaration
/// (followed by a newline); content drops it.
/// Drop whitespace-only text nodes that sit among element siblings
/// (libxml keep-blanks-off behavior when indenting).
fn strip_ws_for_indent(nodes: &mut Vec<Node>) {
    let has_elem = nodes.iter().any(|n| matches!(n, Node::Element(_)));
    if has_elem {
        nodes.retain(|n| !matches!(n, Node::Text(t) if t.trim().is_empty()));
    }
    for n in nodes.iter_mut() {
        if let Node::Element(e) = n {
            strip_ws_for_indent(&mut e.children);
        }
    }
}

pub(crate) fn indent_format(doc_in: &Doc, document: bool) -> String {
    let mut doc = doc_in.clone();
    strip_ws_for_indent(&mut doc.nodes);
    let doc = &doc;
    let mut out = String::new();
    if document {
        if let Some(d) = &doc.decl {
            out.push_str(&serialize_decl(d));
            out.push('\n');
        }
        if let Some(dt) = &doc.doctype {
            out.push_str(dt);
            out.push('\n');
        }
    } else if let Some(dt) = &doc.doctype {
        out.push_str(dt);
        out.push('\n');
    }
    let nodes = &doc.nodes;
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 && matches!(n, Node::Element(_)) {
            out.push('\n');
        }
        format_node(n, 0, &mut out);
    }
    out
}

/// PG xpath(): evaluate and convert to an xml[] Value.
pub(crate) fn eval_xpath_value(
    xpath_text: &str,
    doc_text: &str,
    nsmap: &[(String, String)],
) -> Result<Value, ExecError> {
    let doc = parse_xml_strict_ns(doc_text, false).map_err(|e| e.to_exec())?;
    let expr = compile_xpath(xpath_text)?;
    let v = eval_xpath(&doc, &expr, nsmap)?;
    let strings = xpath_result_strings(&doc, &v, nsmap);
    let elems: Vec<Value> = strings.into_iter().map(Value::xml).collect();
    Ok(Value::Array(Box::new(crate::storage::ArrayVal {
        elem: crate::storage::ArrayElem::Xml,
        dims: vec![elems.len() as i32],
        lower: vec![1],
        elems,
    })))
}

/// PG xpath_exists(): true iff xpath() would return at least one
/// item — a non-empty node set, or ANY scalar result (xpath_internal
/// counts a scalar as one item, even 0 / '' / false).
pub(crate) fn eval_xpath_exists(
    xpath_text: &str,
    doc_text: &str,
    nsmap: &[(String, String)],
) -> Result<bool, ExecError> {
    let doc = parse_xml_strict_ns(doc_text, false).map_err(|e| e.to_exec())?;
    let expr = compile_xpath(xpath_text)?;
    let v = eval_xpath(&doc, &expr, nsmap)?;
    Ok(match &v {
        XpValue::Nodes(ns) => !ns.is_empty(),
        _ => true,
    })
}

/// Extract an ns-array argument (text[][]) into prefix/uri pairs.
pub(crate) fn nsmap_of(v: &Value) -> Result<Vec<(String, String)>, ExecError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(arr) => {
            let mut out = Vec::new();
            // 2-D array: dims [n][2]; elems in row-major order.
            if arr.dims.len() == 2 {
                if arr.dims[1] != 2 {
                    return Err(err(
                        "22023",
                        "namespace array must have two columns per row",
                    ));
                }
                for pair in arr.elems.chunks(2) {
                    let p = pair[0].to_text().unwrap_or_default();
                    let u = pair[1].to_text().unwrap_or_default();
                    out.push((p, u));
                }
            } else {
                return Err(err("22023", "namespace array must be two-dimensional"));
            }
            Ok(out)
        }
        other => Err(err(
            "42804",
            format!(
                "namespace argument must be a text array, not {}",
                other.type_name()
            ),
        )),
    }
}

/// The xml text of a function argument: xml values contribute their
/// stored text; text values are taken as-is (PG coerces unknown/text
/// via xml input in strict contexts by callers that need it).
pub(crate) fn arg_xml_text(v: &Value) -> Result<Option<String>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::Xml(t) => Ok(Some(t.to_string())),
        Value::Text(t) | Value::BpChar(t) => Ok(Some(t.to_string())),
        other => Err(err(
            "42804",
            format!("argument must be of type xml, not {}", other.type_name()),
        )),
    }
}

// -------------------------------------------------- *_to_xml mappings

const XSI_NS: &str = "http://www.w3.org/2001/XMLSchema-instance";

fn xmldata_root_start(out: &mut String, name: &str, top_level: bool, targetns: &str) {
    out.push('<');
    out.push_str(name);
    if top_level {
        out.push_str(" xmlns:xsi=\"");
        out.push_str(XSI_NS);
        out.push('"');
        if !targetns.is_empty() {
            out.push_str(" xmlns=\"");
            out.push_str(targetns);
            out.push('"');
        }
    }
    out.push_str(">\n");
}

/// One column value in a table/query mapping (PG
/// map_sql_value_to_xml_value with escaping): xml values contribute
/// raw markup, arrays become <element> children, other scalars are
/// escaped text.
fn mapped_value(v: &Value) -> Result<String, ExecError> {
    match v {
        Value::Xml(t) => Ok(t.to_string()),
        Value::Array(arr) => {
            let mut out = String::new();
            for e in &arr.elems {
                if *e == Value::Null {
                    continue;
                }
                out.push_str("<element>");
                out.push_str(&mapped_value(e)?);
                out.push_str("</element>");
            }
            Ok(out)
        }
        other => Ok(xml_encode_special(
            &scalar_xml_text(other)?.unwrap_or_default(),
        )),
    }
}

fn mapped_columns(
    out: &mut String,
    cols: &[String],
    row: &[Value],
    nulls: bool,
) -> Result<(), ExecError> {
    for (name, v) in cols.iter().zip(row.iter()) {
        let mapped = map_sql_identifier_to_xml_name_fully(name);
        if *v == Value::Null {
            if nulls {
                out.push_str("  <");
                out.push_str(&mapped);
                out.push_str(" xsi:nil=\"true\"/>\n");
            }
        } else {
            out.push_str("  <");
            out.push_str(&mapped);
            out.push('>');
            out.push_str(&mapped_value(v)?);
            out.push_str("</");
            out.push_str(&mapped);
            out.push_str(">\n");
        }
    }
    Ok(())
}

/// PG query_to_xml_internal / table_to_xml_internal body: map a
/// result set to XML. `tablename` None gives query_to_xml's "table"
/// root (or "row" roots in forest mode).
pub(crate) fn map_result_to_xml(
    tablename: Option<&str>,
    top_level: bool,
    nulls: bool,
    tableforest: bool,
    targetns: &str,
    cols: &[String],
    rows: &[Vec<Value>],
) -> Result<String, ExecError> {
    let mut out = String::new();
    if tableforest {
        let xmltn = match tablename {
            Some(t) => map_sql_identifier_to_xml_name_fully(t),
            None => "row".to_string(),
        };
        for row in rows {
            xmldata_root_start(&mut out, &xmltn, top_level, targetns);
            mapped_columns(&mut out, cols, row, nulls)?;
            out.push_str("</");
            out.push_str(&xmltn);
            out.push_str(">\n\n");
        }
        return Ok(out);
    }
    let xmltn = match tablename {
        Some(t) => map_sql_identifier_to_xml_name_fully(t),
        None => "table".to_string(),
    };
    xmldata_root_start(&mut out, &xmltn, top_level, targetns);
    out.push('\n');
    for row in rows {
        out.push_str("<row>\n");
        mapped_columns(&mut out, cols, row, nulls)?;
        out.push_str("</row>\n\n");
    }
    out.push_str("</");
    out.push_str(&xmltn);
    out.push_str(">\n");
    Ok(out)
}

/// PG schema_to_xml_internal body (xmlschema = None form).
#[allow(clippy::too_many_arguments)]
pub(crate) fn map_schema_to_xml(
    schema: &str,
    nulls: bool,
    tableforest: bool,
    targetns: &str,
    tables: &[(String, Vec<String>, Vec<Vec<Value>>)],
) -> Result<String, ExecError> {
    let xmlsn = map_sql_identifier_to_xml_name_fully(schema);
    let mut out = String::new();
    xmldata_root_start(&mut out, &xmlsn, true, targetns);
    out.push('\n');
    for (tname, cols, rows) in tables {
        out.push_str(&map_result_to_xml(
            Some(tname),
            false,
            nulls,
            tableforest,
            targetns,
            cols,
            rows,
        )?);
        out.push('\n');
    }
    out.push_str("</");
    out.push_str(&xmlsn);
    out.push_str(">\n");
    Ok(out)
}
