/// v1.93: BIT/VARBIT remainder — varbit.c operators (& | # ~ << >>
/// ||), the bit string functions (length/bit_length/octet_length,
/// substring, position, overlay, get_bit/set_bit MSB-first,
/// bit_count), bit_and/bit_or/bit_xor aggregates, bit column typmod
/// enforcement on INSERT/UPDATE, and the GROUP BY capability-mix
/// 0A000 check (bit sort-only vs xid hash-only).
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

#[test]
fn v193_bitwise_ops() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT B'0101' & B'0011'").as_deref(),
        Some("0001")
    );
    assert_eq!(
        cell(&mut eng, "SELECT B'0101' | B'0011'").as_deref(),
        Some("0111")
    );
    assert_eq!(
        cell(&mut eng, "SELECT B'0101' # B'0011'").as_deref(),
        Some("0110")
    );
    // ~ inverts and re-zeros the pad bits (bitlen 5 -> one byte).
    assert_eq!(cell(&mut eng, "SELECT ~B'01010'").as_deref(), Some("10101"));
    assert_eq!(cell(&mut eng, "SELECT ~B'0'").as_deref(), Some("1"));
    // Mismatched lengths: 22026 with PG's per-operator messages.
    let e = err(&mut eng, "SELECT B'001' & B'10'");
    assert_eq!(e.code, "22026");
    assert_eq!(e.message, "cannot AND bit strings of different sizes");
    let e = err(&mut eng, "SELECT B'0111' | B'011'");
    assert_eq!(e.code, "22026");
    assert_eq!(e.message, "cannot OR bit strings of different sizes");
    let e = err(&mut eng, "SELECT B'0010' # B'011101'");
    assert_eq!(e.code, "22026");
    assert_eq!(e.message, "cannot XOR bit strings of different sizes");
    // NULL propagates.
    assert_eq!(cell(&mut eng, "SELECT B'01' & NULL"), None);
    assert_eq!(cell(&mut eng, "SELECT ~NULL::bit"), None);
}

#[test]
fn v193_bit_shifts() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT B'11011000' << 4").as_deref(),
        Some("10000000")
    );
    assert_eq!(
        cell(&mut eng, "SELECT B'11011000' >> 2").as_deref(),
        Some("00110110")
    );
    // Negative counts reverse direction.
    assert_eq!(
        cell(&mut eng, "SELECT B'11011000' << -2").as_deref(),
        Some("00110110")
    );
    assert_eq!(
        cell(&mut eng, "SELECT B'11011000' >> -4").as_deref(),
        Some("10000000")
    );
    // |shift| >= bitlen -> all zeros; length preserved either way.
    assert_eq!(
        cell(&mut eng, "SELECT B'1111' << 4").as_deref(),
        Some("0000")
    );
    assert_eq!(
        cell(&mut eng, "SELECT B'1111' >> 100").as_deref(),
        Some("0000")
    );
    assert_eq!(cell(&mut eng, "SELECT B'101' << 0").as_deref(), Some("101"));
    // Non-byte-boundary lengths.
    assert_eq!(
        cell(&mut eng, "SELECT B'11011' >> 1").as_deref(),
        Some("01101")
    );
    assert_eq!(
        cell(&mut eng, "SELECT B'11011' << 1").as_deref(),
        Some("10110")
    );
}

#[test]
fn v193_bit_concat_and_lengths() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT B'110' || B'01'").as_deref(),
        Some("11001")
    );
    // Non-byte-boundary splice.
    assert_eq!(
        cell(&mut eng, "SELECT B'11011' || B'001'").as_deref(),
        Some("11011001")
    );
    assert_eq!(
        cell(&mut eng, "SELECT length(B'01010')").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "SELECT char_length(B'01010')").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_length(B'01010')").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "SELECT octet_length(B'010101010')").as_deref(),
        Some("2")
    );
    assert_eq!(
        cell(&mut eng, "SELECT octet_length(B'')").as_deref(),
        Some("0")
    );
}

#[test]
fn v193_bit_substring() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT SUBSTRING(B'01010101' FROM 2 FOR 4)").as_deref(),
        Some("1010")
    );
    assert_eq!(
        cell(&mut eng, "SELECT SUBSTRING(B'01010101' FROM 6)").as_deref(),
        Some("101")
    );
    assert_eq!(
        cell(&mut eng, "SELECT substring(B'01010101', 2, 4)").as_deref(),
        Some("1010")
    );
    // Start < 1 shifts the window (PG bitsubstring).
    assert_eq!(
        cell(
            &mut eng,
            "SELECT SUBSTRING('01010101'::bit(8) FROM -10 FOR 2147483646)"
        )
        .as_deref(),
        Some("01010101")
    );
    // s + l overflowing int32 runs to the end.
    assert_eq!(
        cell(
            &mut eng,
            "SELECT SUBSTRING('01010101'::bit(8) FROM 2 FOR 2147483646)"
        )
        .as_deref(),
        Some("1010101")
    );
    // Negative length: 22011.
    let e = err(
        &mut eng,
        "SELECT SUBSTRING('01010101'::bit(8) FROM -10 FOR -2147483646)",
    );
    assert_eq!(e.code, "22011");
    assert_eq!(e.message, "negative substring length not allowed");
    // Window entirely past the end -> zero-length bit string.
    assert_eq!(
        cell(&mut eng, "SELECT SUBSTRING(B'0101' FROM 9 FOR 2)").as_deref(),
        Some("")
    );
}

#[test]
fn v193_bit_position() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "SELECT POSITION(B'1010' IN B'0000101')").as_deref(),
        Some("0")
    );
    assert_eq!(
        cell(&mut eng, "SELECT POSITION(B'1010' IN B'00001010')").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "SELECT POSITION(B'' IN B'00001010')").as_deref(),
        Some("1")
    );
    assert_eq!(
        cell(&mut eng, "SELECT POSITION(B'0' IN B'')").as_deref(),
        Some("0")
    );
    // Empty string beats the empty-substring rule (PG checks it first).
    assert_eq!(
        cell(&mut eng, "SELECT POSITION(B'' IN B'')").as_deref(),
        Some("0")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT POSITION(B'101101' IN B'001011011011011000')"
        )
        .as_deref(),
        Some("3")
    );
    // Substring longer than string -> 0.
    assert_eq!(
        cell(&mut eng, "SELECT POSITION(B'111010110' IN B'11101011')").as_deref(),
        Some("0")
    );
}

#[test]
fn v193_bit_overlay() {
    let mut eng = engine();
    // The placing argument is an unknown literal ('001') resolved to bit.
    assert_eq!(
        cell(
            &mut eng,
            "SELECT overlay(B'0101011100' placing '001' from 2 for 3)"
        )
        .as_deref(),
        Some("0001011100")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT overlay(B'0101011100' placing '101' from 6)"
        )
        .as_deref(),
        Some("0101010100")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT overlay(B'0101011100' placing '001' from 11)"
        )
        .as_deref(),
        Some("0101011100001")
    );
    assert_eq!(
        cell(
            &mut eng,
            "SELECT overlay(B'0101011100' placing '001' from 20)"
        )
        .as_deref(),
        Some("0101011100001")
    );
    // sp <= 0 -> 22011 (bit form, unlike the clamped text form).
    let e = err(&mut eng, "SELECT overlay(B'0101' placing '0' from 0 for 1)");
    assert_eq!(e.code, "22011");
}

#[test]
fn v193_bit_get_set_count() {
    let mut eng = engine();
    // MSB-first numbering: index 10 of 0101011000100 is 1.
    assert_eq!(
        cell(&mut eng, "SELECT get_bit(B'0101011000100', 10)").as_deref(),
        Some("1")
    );
    assert_eq!(
        cell(&mut eng, "SELECT get_bit(B'0101011000100', 0)").as_deref(),
        Some("0")
    );
    assert_eq!(
        cell(&mut eng, "SELECT set_bit(B'0101011000100100', 15, 1)").as_deref(),
        Some("0101011000100101")
    );
    assert_eq!(
        cell(&mut eng, "SELECT set_bit(B'1111', 1, 0)").as_deref(),
        Some("1011")
    );
    let e = err(&mut eng, "SELECT set_bit(B'0101011000100100', 16, 1)");
    assert_eq!(e.code, "2202E");
    assert_eq!(e.message, "bit index 16 out of valid range (0..15)");
    let e = err(&mut eng, "SELECT get_bit(B'1010', -1)");
    assert_eq!(e.code, "2202E");
    assert_eq!(e.message, "bit index -1 out of valid range (0..3)");
    let e = err(&mut eng, "SELECT set_bit(B'1010', 1, 2)");
    assert_eq!(e.code, "22023");
    assert_eq!(e.message, "new bit must be 0 or 1");
    assert_eq!(
        cell(&mut eng, "SELECT bit_count(B'0101011100'::bit(10))").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_count(B'1111111111'::bit(10))").as_deref(),
        Some("10")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_count(repeat('01', 500)::bit(1000))").as_deref(),
        Some("500")
    );
    // The bytea get_bit keeps its LSB-first numbering (bit 0 = LSB of
    // byte 0): '\x01' has bit 0 set, bit 7 clear.
    assert_eq!(
        cell(&mut eng, "SELECT get_bit('\\x01'::bytea, 0)").as_deref(),
        Some("1")
    );
    assert_eq!(
        cell(&mut eng, "SELECT get_bit('\\x80'::bytea, 0)").as_deref(),
        Some("0")
    );
    assert_eq!(
        cell(&mut eng, "SELECT get_bit('\\x80'::bytea, 7)").as_deref(),
        Some("1")
    );
}

#[test]
fn v193_bit_aggregates() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ba (i int, b bit(4))").unwrap();
    run(
        &mut eng,
        "INSERT INTO ba VALUES (12, B'1100'), (10, B'1010'), (NULL, NULL)",
    )
    .unwrap();
    assert_eq!(
        cell(&mut eng, "SELECT bit_and(i) FROM ba").as_deref(),
        Some("8")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_or(i) FROM ba").as_deref(),
        Some("14")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_xor(i) FROM ba").as_deref(),
        Some("6")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_and(b) FROM ba").as_deref(),
        Some("1000")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_or(b) FROM ba").as_deref(),
        Some("1110")
    );
    assert_eq!(
        cell(&mut eng, "SELECT bit_xor(b) FROM ba").as_deref(),
        Some("0110")
    );
    // Zero rows -> NULL.
    run(&mut eng, "CREATE TABLE bae (i int, b bit(4))").unwrap();
    assert_eq!(cell(&mut eng, "SELECT bit_and(i) FROM bae"), None);
    assert_eq!(cell(&mut eng, "SELECT bit_or(b) FROM bae"), None);
    // Smallint inputs keep the smallint fold semantics.
    assert_eq!(
        cell(
            &mut eng,
            "SELECT bit_and(x) FROM (VALUES (7::smallint), (3::smallint)) t(x)"
        )
        .as_deref(),
        Some("3")
    );
    // Bit aggregate over mismatched lengths -> 22026 (scalar transfn).
    run(&mut eng, "CREATE TABLE bav (v bit varying)").unwrap();
    run(&mut eng, "INSERT INTO bav VALUES (B'101'), (B'10')").unwrap();
    let e = err(&mut eng, "SELECT bit_and(v) FROM bav");
    assert_eq!(e.code, "22026");
    // Wrong argument type -> 42883.
    let e = err(&mut eng, "SELECT bit_and('x') FROM ba");
    assert_eq!(e.code, "42883");
}

#[test]
fn v193_bit_typmod_insert() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE bt (b bit(11))").unwrap();
    // Too short for fixed bit: 22026 with PG's exact message.
    let e = err(&mut eng, "INSERT INTO bt VALUES (B'10')");
    assert_eq!(e.code, "22026");
    assert_eq!(e.message, "bit string length 2 does not match type bit(11)");
    // Too long likewise.
    let e = err(&mut eng, "INSERT INTO bt VALUES (B'101011111010')");
    assert_eq!(e.code, "22026");
    assert_eq!(
        e.message,
        "bit string length 12 does not match type bit(11)"
    );
    run(&mut eng, "INSERT INTO bt VALUES (B'00000000000')").unwrap();
    run(&mut eng, "INSERT INTO bt VALUES (B'11011000000')").unwrap();
    assert_eq!(rows(&mut eng, "SELECT * FROM bt").len(), 2);

    run(&mut eng, "CREATE TABLE vt (v bit varying(11))").unwrap();
    run(&mut eng, "INSERT INTO vt VALUES (B'')").unwrap();
    run(&mut eng, "INSERT INTO vt VALUES (B'01010101010')").unwrap();
    let e = err(&mut eng, "INSERT INTO vt VALUES (B'101011111010')");
    assert_eq!(e.code, "22026");
    assert_eq!(e.message, "bit string too long for type bit varying(11)");
    assert_eq!(rows(&mut eng, "SELECT * FROM vt").len(), 2);

    // Bare bit is bit(1).
    run(&mut eng, "CREATE TABLE b1 (b bit)").unwrap();
    run(&mut eng, "INSERT INTO b1 VALUES (B'1')").unwrap();
    let e = err(&mut eng, "INSERT INTO b1 VALUES (B'10')");
    assert_eq!(e.code, "22026");

    // INSERT..SELECT goes through the same final-row check.
    let e = err(&mut eng, "INSERT INTO bt SELECT B'101'");
    assert_eq!(e.code, "22026");
    // UPDATE assignment too (shared check_row_constraints choke point).
    let e = err(&mut eng, "UPDATE bt SET b = B'1'");
    assert_eq!(e.code, "22026");
    run(&mut eng, "UPDATE bt SET b = B'10101010101'").unwrap();

    // Text-literal input is judged post-coercion.
    run(&mut eng, "CREATE TABLE bx (b bit(16))").unwrap();
    run(&mut eng, "INSERT INTO bx VALUES ('X0F00')").unwrap();
    assert_eq!(
        cell(&mut eng, "SELECT b FROM bx").as_deref(),
        Some("0000111100000000")
    );

    // DEFAULT literals of the exact length insert fine.
    run(
        &mut eng,
        "CREATE TABLE bd (b1 bit(4) DEFAULT '1001', b2 bit varying(5) DEFAULT B'0101')",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO bd DEFAULT VALUES").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT b1, b2 FROM bd"),
        vec![vec![Some("1001".to_string()), Some("0101".to_string())]]
    );

    // ALTER TABLE ADD COLUMN carries the typmod too.
    run(&mut eng, "CREATE TABLE at (i int)").unwrap();
    run(&mut eng, "ALTER TABLE at ADD COLUMN b bit(4)").unwrap();
    run(&mut eng, "INSERT INTO at VALUES (1, B'1010')").unwrap();
    let e = err(&mut eng, "INSERT INTO at VALUES (2, B'101')");
    assert_eq!(e.code, "22026");
}

#[test]
fn v193_groupby_capability_mix() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE g (id int, v int, unhashable_col bit(4), unsortable_col xid)",
    )
    .unwrap();
    // (a) one set mixing sort-only (bit) and hash-only (xid): 0A000.
    let e = err(
        &mut eng,
        "SELECT count(*) FROM g GROUP BY rollup(unhashable_col, unsortable_col)",
    );
    assert_eq!(e.code, "0A000");
    assert_eq!(e.message, "could not implement GROUP BY");
    // Plain (non-sets) GROUP BY with the same mix errors too.
    let e = err(
        &mut eng,
        "SELECT count(*) FROM g GROUP BY unhashable_col, unsortable_col",
    );
    assert_eq!(e.code, "0A000");
    // (b) grouping sets with a hash-only key + an ordered aggregate.
    let e = err(
        &mut eng,
        "SELECT array_agg(v ORDER BY v) FROM g GROUP BY grouping sets ((id, unsortable_col), (id))",
    );
    assert_eq!(e.code, "0A000");
    // Must keep passing: mixed sets where each set is single-kind,
    // with unordered aggregates.
    run(
        &mut eng,
        "SELECT count(*), sum(v) FROM g GROUP BY grouping sets ((v, unhashable_col), (v, unsortable_col))",
    )
    .unwrap();
    // A hash-only key alone with an ordered aggregate is fine (single
    // set, not grouping-sets mode).
    run(
        &mut eng,
        "SELECT array_agg(v ORDER BY v) FROM g GROUP BY unsortable_col",
    )
    .unwrap();
    // Sort-only keys with an ordered aggregate over grouping sets: PG
    // can sort, so no error.
    run(
        &mut eng,
        "SELECT array_agg(v ORDER BY v) FROM g GROUP BY grouping sets ((id, unhashable_col), (id))",
    )
    .unwrap();
}

#[test]
fn v193_bit_helpers_direct() {
    // bitsubstring int32-overflow and overlay overflow edges, direct.
    let bs = BitString::parse('b', "01010101").unwrap();
    let r = bit_substring(&bs, 2, Some(2147483646)).unwrap();
    assert_eq!(r.to_bit_chars(), "1010101");
    let r = bit_substring(&bs, -10, Some(2147483646)).unwrap();
    assert_eq!(r.to_bit_chars(), "01010101");
    let e = bit_overlay(&bs, &bs, i32::MAX as i64, i32::MAX as i64).unwrap_err();
    assert_eq!(e.code, "22003");
    // bit_shift clamps absurd counts to all-zeros.
    let r = bit_shift(&bs, i64::MAX, true);
    assert_eq!(r.to_bit_chars(), "00000000");
}
