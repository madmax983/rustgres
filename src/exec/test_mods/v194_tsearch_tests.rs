/// v1.94: TEXT SEARCH TYPES — tsvector/tsquery values, parsers and
/// renderers (tsvector_parser.c / tsquery.c), the english Snowball
/// stemmer + stop words, to_tsvector/to_tsquery family, the @@
/// operator (incl. phrase execution), ts_rank/ts_rank_cd (tsrank.c),
/// and the tsvector utility functions. Ground truth: PG19
/// src/test/regress/expected/tstypes.out and tsearch.out.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
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

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn cell(eng: &mut Engine, sql: &str) -> Option<String> {
    rows(eng, sql)
        .first()
        .and_then(|r| r.iter().next())
        .cloned()
        .flatten()
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Err(e) => e,
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    }
}

fn rank_cell(eng: &mut Engine, sql: &str) -> f32 {
    cell(eng, sql)
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or_else(|| panic!("no rank for {sql}"))
}

fn assert_rank(eng: &mut Engine, sql: &str, want: f32) {
    let got = rank_cell(eng, sql);
    assert!((got - want).abs() <= 2e-6, "{sql}: got {got}, want {want}");
}

#[test]
fn v194_tsvector_parse_render() {
    let mut eng = engine();
    // Sorting, dedupe, position merge (tstypes.out shapes).
    assert_eq!(
        cell(&mut eng, "SELECT 'b:2 a:1'::tsvector").as_deref(),
        Some("'a':1 'b':2")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a:1,3 a:2'::tsvector").as_deref(),
        Some("'a':1,2,3")
    );
    // D weight renders bare; other weights as letters.
    assert_eq!(
        cell(&mut eng, "SELECT 'a:1A,2B,3C,4'::tsvector").as_deref(),
        Some("'a':1A,2B,3C,4")
    );
    // Quoted lexeme with escapes round-trips: the input lexeme
    // 'a\\b' parses to a\b, and tsvectorout doubles the backslash
    // again (oracle-verified against PG19).
    assert_eq!(
        cell(&mut eng, r"SELECT '''a\\b'':1'::tsvector").as_deref(),
        Some(r"'a\\b':1")
    );
    // Stripped entries keep no positions.
    assert_eq!(
        cell(&mut eng, "SELECT 'a b:2'::tsvector").as_deref(),
        Some("'a' 'b':2")
    );
    // Empty vector renders empty.
    assert_eq!(cell(&mut eng, "SELECT ''::tsvector").as_deref(), Some(""));
}

#[test]
fn v194_tsvector_errors() {
    let mut eng = engine();
    let e = err(&mut eng, "SELECT '''' '1'::tsvector");
    assert_eq!(e.code, "42601");
    let e = err(&mut eng, "SELECT 'a:0'::tsvector");
    assert_eq!(e.code, "42601");
    assert!(e.message.contains("wrong position info"), "{}", e.message);
    let e = err(&mut eng, "SELECT 'a:1,'::tsvector");
    assert_eq!(e.code, "42601");
}

#[test]
fn v194_tsquery_parse_render() {
    let mut eng = engine();
    // tstypes.out render shapes.
    assert_eq!(
        cell(&mut eng, "SELECT '1&(2)'::tsquery").as_deref(),
        Some("'1' & '2'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT '!(1)&2'::tsquery").as_deref(),
        Some("!'1' & '2'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT '!(1&2)'::tsquery").as_deref(),
        Some("!( '1' & '2' )")
    );
    assert_eq!(
        cell(&mut eng, "SELECT '1|2&3'::tsquery").as_deref(),
        Some("'1' | '2' & '3'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT '(1|2)&3'::tsquery").as_deref(),
        Some("( '1' | '2' ) & '3'")
    );
    // Weights render in A,B,C,D bit order; prefix star after colon.
    assert_eq!(
        cell(&mut eng, "SELECT 'a:dc'::tsquery").as_deref(),
        Some("'a':CD")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a:*AB'::tsquery").as_deref(),
        Some("'a':*AB")
    );
    // Phrase operators.
    assert_eq!(
        cell(&mut eng, "SELECT 'a <-> b'::tsquery").as_deref(),
        Some("'a' <-> 'b'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a <3> b'::tsquery").as_deref(),
        Some("'a' <3> 'b'")
    );
    // Empty query renders empty.
    assert_eq!(cell(&mut eng, "SELECT ''::tsquery").as_deref(), Some(""));
}

#[test]
fn v194_tsquery_errors() {
    let mut eng = engine();
    let e = err(&mut eng, "SELECT 'a &'::tsquery");
    assert_eq!(e.code, "42601");
    // PG19 tsquery.c: a dangling operator reports "no operand"
    // (oracle-verified); a leading operator is the "syntax error" form.
    assert_eq!(e.message, "no operand in tsquery: \"a &\"");
    let e = err(&mut eng, "SELECT 'a <100000> b'::tsquery");
    assert_eq!(e.code, "22023");
    assert_eq!(
        e.message,
        "distance in phrase operator must be an integer value between zero and 16384 inclusive"
    );
    let e = err(&mut eng, "SELECT '& a'::tsquery");
    assert_eq!(e.code, "42601");
}

#[test]
fn v194_stemmer() {
    // Snowball english (stem_UTF_8_english.c) spot checks — expected
    // values verified against a real PG19 instance (the vendored
    // libstemmer stems relational/conflated/troubled/agreed to
    // relat/conflat/troubl/agre, not the Porter2 paper's forms).
    let cases = [
        ("caresses", "caress"),
        ("ponies", "poni"),
        ("ties", "tie"),
        ("gas", "gas"),
        ("gaps", "gap"),
        ("running", "run"),
        ("happy", "happi"),
        ("skies", "sky"),
        ("spaceships", "spaceship"),
        ("striking", "strike"),
        ("news", "news"),
        ("dying", "die"),
        ("lying", "lie"),
        ("ugly", "ugli"),
        ("early", "earli"),
        ("relational", "relat"),
        ("cats", "cat"),
        ("agreed", "agre"),
        ("motoring", "motor"),
        ("conflated", "conflat"),
        ("troubled", "troubl"),
        ("sized", "size"),
        ("hopping", "hop"),
        ("tanned", "tan"),
        ("falling", "fall"),
        ("hissing", "hiss"),
        ("fizzed", "fizz"),
        ("failing", "fail"),
        ("filing", "file"),
    ];
    for (input, want) in cases {
        assert_eq!(super::tsearch::english_stem(input), want, "stem({input})");
    }
}

#[test]
fn v194_to_tsvector_english() {
    let mut eng = engine();
    // tstypes.out: stop words consume positions (from=4, a=5).
    assert_eq!(
        cell(
            &mut eng,
            "SELECT to_tsvector('english', 'Rebel spaceships, striking from a hidden base')"
        )
        .as_deref(),
        Some("'base':7 'hidden':6 'rebel':1 'spaceship':2 'strike':3")
    );
    // Default config is english.
    assert_eq!(
        cell(&mut eng, "SELECT to_tsvector('The Fat Rats')").as_deref(),
        Some("'fat':2 'rat':3")
    );
    // Simple config: lowercase only, no stemming, no stop words.
    assert_eq!(
        cell(&mut eng, "SELECT to_tsvector('simple', 'The Fat Rats')").as_deref(),
        Some("'fat':2 'rats':3 'the':1")
    );
    // regconfig OID form for `simple` (3748).
    assert_eq!(
        cell(&mut eng, "SELECT to_tsvector(3748, 'Abc Def')").as_deref(),
        Some("'abc':1 'def':2")
    );
    // Unknown configuration name: 42704 with PG's message.
    let e = err(&mut eng, "SELECT to_tsvector('nope', 'x')");
    assert_eq!(e.code, "42704");
    assert_eq!(
        e.message,
        "text search configuration \"nope\" does not exist"
    );
}

#[test]
fn v194_to_tsquery_family() {
    let mut eng = engine();
    // tsearch.out shapes.
    assert_eq!(
        cell(&mut eng, "SELECT to_tsquery('english', 'qwe & sKies ')").as_deref(),
        Some("'qwe' & 'sky'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT to_tsquery('simple', 'qwe & sKies ')").as_deref(),
        Some("'qwe' & 'skies'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT to_tsquery('english', 'asd&(and|fghj)')").as_deref(),
        Some("'asd' & 'fghj'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT to_tsquery('english', '(asd&!and)|fghj')").as_deref(),
        Some("'asd' | 'fghj'")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT plainto_tsquery('english', 'The Fat Rats')"
        )
        .as_deref(),
        Some("'fat' & 'rat'")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT phraseto_tsquery('english', 'The Fat Rats')"
        )
        .as_deref(),
        Some("'fat' <-> 'rat'")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT websearch_to_tsquery('english', '\"sad cat\" or fat rat')"
        )
        .as_deref(),
        Some("'sad' <-> 'cat' | 'fat' & 'rat'")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT websearch_to_tsquery('english', 'fat -rat')"
        )
        .as_deref(),
        Some("'fat' & !'rat'")
    );
    // 1-arg forms use the default (english) config.
    assert_eq!(
        cell(&mut eng, "SELECT plainto_tsquery('fat rats')").as_deref(),
        Some("'fat' & 'rat'")
    );
}

#[test]
fn v194_match_operator() {
    let mut eng = engine();
    let t = |eng: &mut Engine, sql: &str| cell(eng, sql).as_deref() == Some("t");
    // Basic boolean structure.
    assert!(t(
        &mut eng,
        "SELECT 'a:1 b:2'::tsvector @@ 'a & b'::tsquery"
    ));
    assert!(!t(
        &mut eng,
        "SELECT 'a:1 b:2'::tsvector @@ 'a & c'::tsquery"
    ));
    assert!(t(&mut eng, "SELECT 'a:1'::tsvector @@ 'a | c'::tsquery"));
    assert!(t(&mut eng, "SELECT 'a:1'::tsvector @@ '!c'::tsquery"));
    assert!(!t(&mut eng, "SELECT 'a:1'::tsvector @@ '!a'::tsquery"));
    // Reversed operand order.
    assert!(t(&mut eng, "SELECT 'a'::tsquery @@ 'a:1'::tsvector"));
    // Prefix matching.
    assert!(t(&mut eng, "SELECT 'abcd:1'::tsvector @@ 'ab:*'::tsquery"));
    assert!(!t(&mut eng, "SELECT 'abcd:1'::tsvector @@ 'ac:*'::tsquery"));
    // Weight-restricted operands (tstypes.out).
    assert!(t(
        &mut eng,
        "SELECT 'wa:1D wb:2A'::tsvector @@ 'w:*D <-> w:*A'::tsquery"
    ));
    assert!(!t(
        &mut eng,
        "SELECT 'wa:1A wb:2D'::tsvector @@ 'w:*D <-> w:*A'::tsquery"
    ));
    // Phrase distance through prefix positions (tstypes.out).
    assert!(t(
        &mut eng,
        "SELECT to_tsvector('simple','1 2 11 3') @@ '1:* <-> 3'::tsquery"
    ));
    assert!(!t(
        &mut eng,
        "SELECT to_tsvector('simple','1 2 11 3') @@ '1 <-> 3'::tsquery"
    ));
    // Phrase with explicit distance.
    assert!(t(
        &mut eng,
        "SELECT 'a:1 b:3'::tsvector @@ 'a <2> b'::tsquery"
    ));
    assert!(!t(
        &mut eng,
        "SELECT 'a:1 b:3'::tsvector @@ 'a <-> b'::tsquery"
    ));
    // Stripped entries: plain match works, phrase match does not.
    assert!(t(
        &mut eng,
        "SELECT strip('a:1 b:2'::tsvector) @@ 'a'::tsquery"
    ));
    assert!(!t(
        &mut eng,
        "SELECT strip('a:1 b:2'::tsvector) @@ 'a <-> b'::tsquery"
    ));
    // Bare-literal coercion on either side of @@.
    assert!(t(&mut eng, "SELECT 'a:1'::tsvector @@ 'a'"));
    assert!(t(&mut eng, "SELECT 'the fat rats' @@ 'fat & rat'::tsquery"));
    // Empty query matches nothing.
    assert!(!t(&mut eng, "SELECT 'a:1'::tsvector @@ ''::tsquery"));
}

#[test]
fn v194_query_operators() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT 'a'::tsquery && 'b'::tsquery").as_deref(),
        Some("'a' & 'b'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a'::tsquery || 'b'::tsquery").as_deref(),
        Some("'a' | 'b'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a'::tsquery <-> 'b'::tsquery").as_deref(),
        Some("'a' <-> 'b'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a'::tsquery <4> 'b'::tsquery").as_deref(),
        Some("'a' <4> 'b'")
    );
    // SQL-level `!!` is PG's tsquery NOT prefix operator (one
    // negation — oracle-verified; the string form '!!a' keeps both).
    assert_eq!(
        cell(&mut eng, "SELECT !!'a'::tsquery").as_deref(),
        Some("!'a'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT '!!a'::tsquery").as_deref(),
        Some("!!'a'")
    );
    // A lone `!` prefix has no operator in PG: 42883.
    let e = err(&mut eng, "SELECT !'a'::tsquery");
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "operator does not exist: ! tsquery");
    assert_eq!(
        cell(
            &mut eng,
            "SELECT tsquery_phrase('a'::tsquery, 'b'::tsquery, 3)"
        )
        .as_deref(),
        Some("'a' <3> 'b'")
    );
    // tsvector concat (tstypes.out): positions offset by left max.
    assert_eq!(
        cell(
            &mut eng,
            "SELECT 'a:3A b:2a'::tsvector || 'ba:1234 a:1B'::tsvector"
        )
        .as_deref(),
        Some("'a':3A,4B 'b':2A 'ba':1237")
    );
    // numnode counts operands + operators.
    assert_eq!(
        cell(&mut eng, "SELECT numnode('a & b | c'::tsquery)").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "SELECT numnode(''::tsquery)").as_deref(),
        Some("0")
    );
}

#[test]
fn v194_utilities() {
    let mut eng = engine();
    // ts_delete, text and text[] forms (tstypes.out).
    assert_eq!(
        cell(
            &mut eng,
            "SELECT ts_delete('base:7 hidden:6 rebel:1 spaceship:2,33A strike:3'::tsvector, 'base')"
        )
        .as_deref(),
        Some("'hidden':6 'rebel':1 'spaceship':2,33A 'strike':3")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT ts_delete('a:1 b:2 c:3'::tsvector, ARRAY['a','c'])"
        )
        .as_deref(),
        Some("'b':2")
    );
    // setweight 2-arg and 3-arg (tstypes.out).
    assert_eq!(
        cell(
            &mut eng,
            "SELECT setweight('a:1,3A asd:1C w:5,6,12B,13A zxc:81,222A,567'::tsvector, 'c')"
        )
        .as_deref(),
        Some("'a':1C,3C 'asd':1C 'w':5C,6C,12C,13C 'zxc':81C,222C,567C")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT setweight('a:1,3A asd:1C w:5,6,12B,13A zxc:81,222A,567'::tsvector, 'c', '{a}')"
        )
        .as_deref(),
        Some("'a':1C,3C 'asd':1C 'w':5,6,12B,13A 'zxc':81,222A,567")
    );
    // strip / length / array conversions.
    assert_eq!(
        cell(&mut eng, "SELECT strip('a:1A b:2'::tsvector)").as_deref(),
        Some("'a' 'b'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT length('a:1 b:2 c'::tsvector)").as_deref(),
        Some("3")
    );
    assert_eq!(
        cell(&mut eng, "SELECT tsvector_to_array('b:2 a:1'::tsvector)").as_deref(),
        Some("{a,b}")
    );
    assert_eq!(
        cell(&mut eng, "SELECT array_to_tsvector(ARRAY['b','a','b'])").as_deref(),
        Some("'a' 'b'")
    );
    // ts_filter keeps only listed weights.
    assert_eq!(
        cell(
            &mut eng,
            "SELECT ts_filter('a:1A,2B b:1C'::tsvector, '{a,b}')"
        )
        .as_deref(),
        Some("'a':1A,2B")
    );
    // array_to_tsvector errors (tsvector_op.c).
    let e = err(&mut eng, "SELECT array_to_tsvector(ARRAY['a', NULL])");
    assert_eq!(e.code, "22004");
    let e = err(&mut eng, "SELECT array_to_tsvector(ARRAY[''])");
    assert_eq!(e.code, "2200F");
    let e = err(&mut eng, "SELECT ts_filter('a:1'::tsvector, '{a,NULL}')");
    assert_eq!(e.code, "22004");
    // setweight with a bad weight letter: 22023.
    let e = err(&mut eng, "SELECT setweight('a:1'::tsvector, 'x')");
    assert_eq!(e.code, "22023");
}

#[test]
fn v194_rank() {
    let mut eng = engine();
    // tstypes.out ts_rank values (default weights, method 0).
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2C d g'::tsvector, 'a | s')",
        0.0911891,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 sa:2C d g'::tsvector, 'a | s')",
        0.0303964,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 sa:2C d g'::tsvector, 'a | s:*')",
        0.0911891,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2B d g'::tsvector, 'a | s')",
        0.151982,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2 d g'::tsvector, 'a | s')",
        0.0607927,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2C d g'::tsvector, 'a & s')",
        0.140153,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2B d g'::tsvector, 'a & s')",
        0.198206,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2 d g'::tsvector, 'a & s')",
        0.0991032,
    );
    // tstypes.out ts_rank_cd values.
    assert_rank(
        &mut eng,
        "SELECT ts_rank_cd(' a:1 s:2C d g'::tsvector, 'a | s')",
        0.3,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank_cd(' a:1 sa:2C d g'::tsvector, 'a | s')",
        0.1,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank_cd(' a:1 sa:2C d g'::tsvector, 'a | s:*')",
        0.3,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank_cd(' a:1 sa:3C sab:2c d g'::tsvector, 'a | sa:*')",
        0.5,
    );
    assert_rank(
        &mut eng,
        "SELECT ts_rank_cd(' a:1 s:2B d g'::tsvector, 'a | s')",
        0.5,
    );
    // Explicit weights array + normalization method shapes parse.
    assert_rank(
        &mut eng,
        "SELECT ts_rank(ARRAY[0.1,0.2,0.4,1.0], ' a:1 s:2C d g'::tsvector, 'a | s')",
        0.0911891,
    );
    // Normalization method 2 divides by cnt_length (4 here):
    // 0.0911891 / 4 (tsrank.c calc_rank).
    assert_rank(
        &mut eng,
        "SELECT ts_rank(' a:1 s:2C d g'::tsvector, 'a | s', 2)",
        0.0227973,
    );
}

#[test]
fn v194_comparisons() {
    let mut eng = engine();
    let t = |eng: &mut Engine, sql: &str| cell(eng, sql).as_deref() == Some("t");
    assert!(t(
        &mut eng,
        "SELECT 'a:1 b:2'::tsvector = 'a:1 b:2'::tsvector"
    ));
    assert!(t(&mut eng, "SELECT 'a:1'::tsvector <> 'a:2'::tsvector"));
    assert!(t(&mut eng, "SELECT 'a:1'::tsvector < 'b:1'::tsvector"));
    assert!(t(&mut eng, "SELECT 'a & b'::tsquery = 'a & b'::tsquery"));
    assert!(t(&mut eng, "SELECT 'a'::tsquery < 'a & b'::tsquery"));
    assert!(t(&mut eng, "SELECT 'a:1'::tsvector = 'a:1'"));
    // Text output casts.
    assert_eq!(
        cell(&mut eng, "SELECT 'a:1'::tsvector::text").as_deref(),
        Some("'a':1")
    );
    assert_eq!(
        cell(&mut eng, "SELECT 'a & b'::tsquery::text").as_deref(),
        Some("'a' & 'b'")
    );
    assert_eq!(
        cell(&mut eng, "SELECT pg_typeof('a:1'::tsvector)").as_deref(),
        Some("tsvector")
    );
    assert_eq!(
        cell(&mut eng, "SELECT pg_typeof(to_tsquery('a'))").as_deref(),
        Some("tsquery")
    );
}

#[test]
fn v194_table_columns_and_unnest() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE tsdocs (id int, doc tsvector, q tsquery)",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO tsdocs VALUES (1, to_tsvector('english', 'The Fat Rats'), to_tsquery('fat & rat')), (2, 'a:1 b:2', 'a <-> b')",
    )
    .unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT id FROM tsdocs WHERE doc @@ q ORDER BY id"),
        vec![vec![Some("1".to_string())], vec![Some("2".to_string())]]
    );
    assert_eq!(
        cell(&mut eng, "SELECT doc FROM tsdocs WHERE id = 2").as_deref(),
        Some("'a':1 'b':2")
    );
    // unnest(tsvector) in FROM: (lexeme, positions, weights) rows.
    assert_eq!(
        rows(&mut eng, "SELECT * FROM unnest('a:1,3A b:2'::tsvector)"),
        vec![
            vec![
                Some("a".to_string()),
                Some("{1,3}".to_string()),
                Some("{D,A}".to_string())
            ],
            vec![
                Some("b".to_string()),
                Some("{2}".to_string()),
                Some("{D}".to_string())
            ],
        ]
    );
    // unnest(tsvector) in the SELECT list renders records.
    assert_eq!(
        cell(&mut eng, "SELECT unnest('base:7'::tsvector)").as_deref(),
        Some("(base,{7},{D})")
    );
    // Stripped entries unnest with NULL positions/weights.
    assert_eq!(
        cell(&mut eng, "SELECT unnest(strip('base:7'::tsvector))").as_deref(),
        Some("(base,,)")
    );
}

#[test]
fn v194_pg_input_validators() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT pg_input_is_valid('a:1', 'tsvector')").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "SELECT pg_input_is_valid('a:0', 'tsvector')").as_deref(),
        Some("f")
    );
    assert_eq!(
        cell(&mut eng, "SELECT pg_input_is_valid('a & b', 'tsquery')").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT pg_input_is_valid('a <100000> b', 'tsquery')"
        )
        .as_deref(),
        Some("f")
    );
    // The error info carries the real parser message + code.
    let info = cell(
        &mut eng,
        "SELECT pg_input_error_info('a <100000> b', 'tsquery')",
    );
    let info = info.unwrap_or_default();
    assert!(
        info.contains(
            "distance in phrase operator must be an integer value between zero and 16384 inclusive"
        ),
        "{info}"
    );
    assert!(info.contains("22023"), "{info}");
}

#[test]
fn v194_direct_parser_units() {
    use super::tsearch::*;
    // Round-trip through the Rust API directly.
    let v = parse_tsvector("'fat':2,3 'rat':1").unwrap();
    assert_eq!(tsvector_to_text(&v), "'fat':2,3 'rat':1");
    let q = parse_tsquery("a & (b | !c)").unwrap();
    assert_eq!(tsquery_to_text(&q), "'a' & ( 'b' | !'c' )");
    // tsvector_cmp (silly_cmp_tsvector): the serialized size decides
    // first, so the vector with more positions is the larger one
    // (oracle-verified: tsvector_cmp('a:1','a:1,2') = -1 in PG19).
    let a = parse_tsvector("a:1").unwrap();
    let b = parse_tsvector("a:1,2").unwrap();
    assert_eq!(tsvector_cmp(&a, &b), std::cmp::Ordering::Less);
    assert_eq!(tsvector_cmp(&b, &a), std::cmp::Ordering::Greater);
    // tsquery_cmp: empty < non-empty.
    let e = parse_tsquery("").unwrap();
    let one = parse_tsquery("a").unwrap();
    assert_eq!(tsquery_cmp(&e, &one), std::cmp::Ordering::Less);
}
