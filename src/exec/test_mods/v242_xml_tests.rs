/// v2.42: SQL/XML subsystem tests — the pure-Rust parser, the
/// XPath 1.0 subset, SQL/XML constructors, and XMLTABLE, exercised
/// through the public helpers in `exec::xml` plus a few end-to-end
/// statements through the statement harness.
use super::xml::*;
use super::*;

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        privs_suppressed: false,
        snap: &snap,
        own: 9,
        write_xid: 9,
        all_xids: vec![9],
        level: IsolationLevel::ReadCommitted,
        writes: &mut writes,
        session: 0,
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn doc(s: &str) -> Doc {
    parse_xml(s, false).expect("parse should succeed")
}

fn xpath_strings(xp: &str, xml: &str) -> Vec<String> {
    let d = doc(xml);
    let e = compile_xpath(xp).expect("xpath should compile");
    let v = eval_xpath(&d, &e, &[]).expect("xpath should evaluate");
    xpath_result_strings(&d, &v, &[])
}

#[test]
fn xml_parse_document_and_content() {
    let d = parse_xml("<a x=\"1\">t<b/></a>", true).unwrap();
    assert!(is_document("<a/>"));
    assert!(!is_document("<a/><b/>"));
    assert_eq!(d.nodes.len(), 1);
    // Content: multiple roots fine.
    assert!(parse_xml("<a/><b/>", false).is_ok());
    // Mismatched tags fail in both modes.
    assert!(parse_xml("<a></b>", false).is_err());
    assert!(parse_xml("<a></b>", true).is_err());
}

#[test]
fn xml_entities_and_doctype() {
    // Internal-subset entity expands.
    let d = doc("<!DOCTYPE a [<!ENTITY x \"hi\">]><a>&x;</a>");
    assert_eq!(string_value(&d.nodes[0]), "hi");
    // External entity is kept unexpanded, not an error.
    let d2 = doc("<!DOCTYPE a [<!ENTITY x SYSTEM \"foo\">]><a>&x;</a>");
    assert!(serialize_doc(&d2).contains("&x;"));
    // Undefined entity without a DOCTYPE is an error.
    assert!(parse_xml("<a>&nosuch;</a>", false).is_err());
}

#[test]
fn xml_undeclared_prefix_is_tolerated() {
    // libxml/PG: an undeclared namespace prefix is not a
    // well-formedness error.
    assert!(parse_xml("<nosuchprefix:tag/>", false).is_ok());
    assert!(parse_xml("<nosuchprefix:tag/>", true).is_ok());
}

#[test]
fn xml_content_with_doctype_needs_document_shape() {
    // PG xml_parse: content containing a DOCTYPE is parsed as a
    // document; xml_cast_in applies the same rule.
    assert!(xml_cast_in("<!DOCTYPE a><a/>").is_ok());
    assert!(xml_cast_in("<!DOCTYPE a><a/><b/>").is_err());
    assert!(xml_cast_in("<!-- c --> oops <!DOCTYPE a><a/>").is_err());
    // CONTENT input strips the declaration from the stored value.
    assert_eq!(
        xml_cast_in("<?xml version=\"1.0\"?> <!-- hi--> <!DOCTYPE a><a/>").unwrap(),
        " <!-- hi--> <!DOCTYPE a><a/>"
    );
}

#[test]
fn xml_comment_and_text_rules() {
    assert_eq!(xml_comment("hello").unwrap(), Value::xml("<!--hello-->"));
    assert_eq!(xml_comment("a--b").unwrap_err().code, "2200S");
    assert_eq!(
        xml_text("a & <\"b\">"),
        Value::xml("a &amp; &lt;&quot;b&quot;&gt;")
    );
}

#[test]
fn xml_concat_declaration_merging() {
    let one = xml_concat(&["<foo/>".to_string()]);
    assert_eq!(one, Value::xml("<foo/>"));
    let two = xml_concat(&[
        "<?xml version=\"1.1\"?><foo/>".to_string(),
        "<?xml version=\"1.1\" standalone=\"no\"?><bar/>".to_string(),
    ]);
    assert_eq!(two, Value::xml("<?xml version=\"1.1\"?><foo/><bar/>"));
    let three = xml_concat(&[
        "<foo/>".to_string(),
        "<?xml version=\"1.1\" standalone=\"no\"?><bar/>".to_string(),
    ]);
    assert_eq!(three, Value::xml("<foo/><bar/>"));
}

#[test]
fn xml_identifier_mapping() {
    assert_eq!(map_sql_identifier_to_xml_name("foo"), "foo");
    assert_eq!(map_sql_identifier_to_xml_name("123"), "_x0031_23");
    assert_eq!(
        map_sql_identifier_to_xml_name(":::_xml_abc135.%-&_"),
        "_x003A_::_x005F_xml_abc135._x0025_-_x0026__"
    );
    assert_eq!(map_sql_identifier_to_xml_name(":a"), "_x003A_a");
}

#[test]
fn xpath_basics() {
    assert_eq!(
        xpath_strings("/a/b/text()", "<a><b>hi</b></a>"),
        vec!["hi".to_string()]
    );
    assert_eq!(
        xpath_strings("//@value", "<root value=\"&lt;\"/>"),
        vec!["&lt;".to_string()]
    );
    assert_eq!(
        xpath_strings("count(/a/*)", "<a><b/><c/></a>"),
        vec!["2".to_string()]
    );
    assert_eq!(
        xpath_strings("/a/b[2]", "<a><b>1</b><b>2</b></a>"),
        vec!["<b>2</b>".to_string()]
    );
}

#[test]
fn xpath_text_predicate() {
    let xml = "<towns><town>Bidford-on-Avon</town><town>Toronto</town></towns>";
    assert_eq!(
        xpath_strings("//town[text() = 'Toronto']", xml),
        vec!["<town>Toronto</town>".to_string()]
    );
    let d = doc(xml);
    let e = compile_xpath("//town[text() = 'Toronto']").unwrap();
    let v = eval_xpath(&d, &e, &[]).unwrap();
    let xp = Xp {
        forest: &d.nodes,
        nsmap: &[],
        default_ns: None,
    };
    assert!(xp.to_bool(&v));
}

#[test]
fn xpath_namespaces() {
    let xml = "<local:data xmlns:local=\"http://127.0.0.1\"><local:piece id=\"1\">n</local:piece></local:data>";
    let nsmap = vec![("loc".to_string(), "http://127.0.0.1".to_string())];
    let d = doc(xml);
    let e = compile_xpath("//loc:piece").unwrap();
    let v = eval_xpath(&d, &e, &nsmap).unwrap();
    // PG19 (xml.out): a node result carries the ancestor in-scope
    // namespace declarations the subtree actually uses.
    assert_eq!(
        xpath_result_strings(&d, &v, &nsmap),
        vec!["<local:piece xmlns:local=\"http://127.0.0.1\" id=\"1\">n</local:piece>".to_string()]
    );
}

#[test]
fn xml_serialize_indent_shape() {
    let d = parse_xml("<foo><bar><val x=\"y\">42</val></bar></foo>", true).unwrap();
    assert_eq!(
        indent_format(&d, true),
        "<foo>\n  <bar>\n    <val x=\"y\">42</val>\n  </bar>\n</foo>"
    );
    // Mixed content stays inline.
    let d2 = parse_xml("<foo>text<bar/>more</foo>", true).unwrap();
    assert_eq!(indent_format(&d2, true), "<foo>text<bar/>more</foo>");
}

#[test]
fn xml_root_variants() {
    assert_eq!(
        xml_root("<foo/>", Some(None), Some(None)).unwrap(),
        "<foo/>"
    );
    assert_eq!(
        xml_root("<foo/>", Some(None), Some(Some(true))).unwrap(),
        "<?xml version=\"1.0\" standalone=\"yes\"?><foo/>"
    );
    assert_eq!(
        xml_root(
            "<?xml version=\"1.1\"?><foo/>",
            Some(Some("1.0".to_string())),
            None
        )
        .unwrap(),
        "<?xml version=\"1.0\"?><foo/>"
    );
}

#[test]
fn xml_end_to_end_statements() {
    let mut eng = Engine::new();
    let r = run(
        &mut eng,
        "SELECT xmlelement(name foo, xmlattributes(1 as one), 'bar')",
    )
    .unwrap();
    match r {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::xml("<foo one=\"1\">bar</foo>"));
        }
        _ => panic!("expected rows"),
    }
    let r = run(&mut eng, "SELECT xpath('/a/b/text()', '<a><b>hi</b></a>')").unwrap();
    match r {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows[0][0].to_text().as_deref(), Some("{hi}"));
        }
        _ => panic!("expected rows"),
    }
}

#[test]
fn v242_table_and_query_to_xml() {
    let mut eng = Engine::new();
    run(&mut eng, "CREATE SCHEMA testxmlschema").unwrap();
    run(&mut eng, "CREATE TABLE testxmlschema.test1 (a int, b text)").unwrap();
    run(
        &mut eng,
        "INSERT INTO testxmlschema.test1 VALUES (1, 'one'), (2, 'two'), (-1, null)",
    )
    .unwrap();
    let res = run(
        &mut eng,
        "SELECT table_to_xml('testxmlschema.test1', false, false, '')",
    )
    .unwrap();
    let ExecResult::Select { rows, .. } = res else {
        panic!("expected select")
    };
    let Value::Xml(t) = &rows[0].clone().into_cells()[0] else {
        panic!("expected xml")
    };
    assert_eq!(
        &**t,
        "<test1 xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n\n<row>\n  <a>1</a>\n  <b>one</b>\n</row>\n\n<row>\n  <a>2</a>\n  <b>two</b>\n</row>\n\n<row>\n  <a>-1</a>\n</row>\n\n</test1>\n"
    );
    let res = run(
        &mut eng,
        "SELECT query_to_xml('SELECT a FROM testxmlschema.test1 WHERE a = 2', true, true, 'ns')",
    )
    .unwrap();
    let ExecResult::Select { rows, .. } = res else {
        panic!("expected select")
    };
    let Value::Xml(t) = &rows[0].clone().into_cells()[0] else {
        panic!("expected xml")
    };
    assert_eq!(
        &**t,
        "<row xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns=\"ns\">\n  <a>2</a>\n</row>\n\n"
    );
}
