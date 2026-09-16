#!/usr/bin/env python3
r"""v0.36 protocol tests: PG's one-byte `"char"` type (OID 18).

RED (base 876feaa, before this milestone): the 8 quoted-`"char"` statements
in tests/conformance/data/expected/char.out are REAL-FAILs -- `"char"` lexed
as a plain identifier, so `'\101'::"char"` went through `char(1)` assignment
(22001) instead of charin, and `'\377'::"char"` could not round-trip byte 255.

GREEN (this milestone):
- `"char"` is a distinct type (OID 18), not `character(1)` (OID 1042).
- Input (charin): `""` -> NUL, `\ooo` -> bytea-style octal escape, any other
  single byte -> itself; longer input keeps the first byte (PG's charin
  semantics), it does NOT raise 22001.
- Output (charout): NUL -> empty, bytes >= 0x80 -> `\ooo` octal, else the byte.
- `"char"` <-> text casts work; comparison is a byte comparison.
- Double-quoted identifiers keep working everywhere (never keywords).

Ground truth: tests/conformance/data/expected/char.out (the 9 `"char"`
statements) for input/output; PG19 char.c semantics for the rest.
"""
import socket, struct, sys

PORT = 5433

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c: raise RuntimeError("closed")
        d += c
    return d

def read_msg(s):
    t = read_exact(s, 1)
    (ln,) = struct.unpack("!i", read_exact(s, 4))
    return t, read_exact(s, ln - 4)

s = socket.create_connection(('127.0.0.1', PORT), timeout=10)
body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
s.sendall(struct.pack("!i", len(body) + 4) + body)
while True:
    t, p = read_msg(s)
    if t == b'Z': break

def do_sql(q):
    s.sendall(b'Q' + struct.pack("!i", len(q.encode()) + 5) + q.encode() + b'\x00')
    rows, err, msg, cols, oids = [], None, None, [], []
    while True:
        t, p = read_msg(s)
        if t == b'T':
            (n,) = struct.unpack("!h", p[:2]); pos = 2
            for _ in range(n):
                e = p.index(b'\x00', pos); cols.append(p[pos:e].decode()); pos = e + 1
                (oid,) = struct.unpack("!i", p[pos+6:pos+10]); oids.append(oid); pos += 18
        elif t == b'D':
            (n,) = struct.unpack("!h", p[:2])
            pos, row = 2, []
            for _ in range(n):
                (ln,) = struct.unpack("!i", p[pos:pos+4]); pos += 4
                if ln == -1: row.append(None)
                else: row.append(p[pos:pos+ln].decode()); pos += ln
            rows.append(row)
        elif t == b'E':
            f, pos = {}, 0
            while pos < len(p) and p[pos] != 0:
                c = chr(p[pos]); pos += 1
                e = p.index(b'\x00', pos); f[c] = p[pos:e].decode('utf8', 'replace'); pos = e + 1
            err = f.get('C'); msg = f.get('M')
        elif t == b'Z':
            break
    return rows, err, msg, cols, oids

def val(q):
    rows, err, msg, _, _ = do_sql(q)
    assert not err, f"{q} -> SQLSTATE {err}: {msg}"
    assert len(rows) == 1 and len(rows[0]) == 1, f"{q} -> {rows}"
    return rows[0][0]

def err_of(q):
    _, err, _, _, _ = do_sql(q)
    return err

def msg_of(q):
    _, _, msg, _, _ = do_sql(q)
    return msg

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

# A. The 9 conformance-pinned "char" statements (char.out).
check("A1 'a'::\"char\"", val("SELECT 'a'::\"char\"") == "a")
check("A2 octal escape in", val("SELECT '\\101'::\"char\"") == "A")
check("A3 high byte in/out", val("SELECT '\\377'::\"char\"") == "\\377")
check("A4 char->text", val("SELECT 'a'::\"char\"::text") == "a")
check("A5 high byte char->text", val("SELECT '\\377'::\"char\"::text") == "\\377")
check("A6 NUL char->text is empty", val("SELECT '\\000'::\"char\"::text") == "")
check("A7 text->char", val("SELECT 'a'::text::\"char\"") == "a")
check("A8 high byte text->char", val("SELECT '\\377'::text::\"char\"") == "\\377")
check("A9 empty text->char is NUL", val("SELECT ''::text::\"char\"::text") == "")

# B. charin edge rules (char.c: first byte wins, never errors).
check("B1 empty in is NUL", val("SELECT ''::\"char\"::text") == "")
check("B2 multi-char takes first byte", val("SELECT 'ab'::\"char\"") == "a")
check("B3 multibyte takes first UTF-8 byte", val("SELECT 'é'::\"char\"") == "\\303")
check("B4 lone backslash is itself", val("SELECT '\\\\'::\"char\"::text") == "\\")
check("B5 short escape takes backslash", val("SELECT '\\12'::\"char\"::text") == "\\")

# B6. int4 <-> "char" casts (chartoi4/i4tochar, explicit only).
check("B6 int->char", val("SELECT 65::\"char\"") == "A")
check("B7 char->int", val("SELECT 'a'::\"char\"::int") == "97")
check("B8 char->int is signed", val("SELECT '\\377'::\"char\"::int") == "-1")
check("B9 neg int->char", val("SELECT (-1)::\"char\"") == "\\377")
check("B10 int->char out of range", err_of("SELECT 200::\"char\"") == "22003")
check("B11 out-of-range message",
      msg_of("SELECT 200::\"char\"") == "\"char\" out of range")

# B12. Implicit "char"->text: length() sees the 4-char escape text.
check("B12 length via implicit cast", val("SELECT length('\\377'::\"char\")") == "4")

# C. Type identity: OID 18, column label, distinct from character(1).
_, _, _, cols, oids = do_sql("SELECT 'a'::\"char\"")
check("C1 wire OID is 18", oids == [18])
check("C2 cast column named char", cols == ["char"])
check("C3 \"char\" is not char(1)",
      val("SELECT CAST('ab' AS char)") == "a" and val("SELECT 'ab'::\"char\"") == "a")
check("C4 no typmod allowed", err_of("SELECT CAST('a' AS \"char\"(1))") is not None)

# D. Table round-trip incl. WAL persistence of the byte value.
do_sql("DROP TABLE IF EXISTS t36_c")
check("D1 create", err_of('CREATE TABLE t36_c (c "char")') is None)
check("D2 insert", err_of("INSERT INTO t36_c VALUES ('\\377'::\"char\")") is None)
check("D3 select round-trips byte 255", val("SELECT c FROM t36_c") == "\\377")
check("D4 select via text", val("SELECT c::text FROM t36_c") == "\\377")
check("D5 assignment uses charin (first byte)", err_of("INSERT INTO t36_c VALUES ('ab')") is None
      and val("SELECT c FROM t36_c WHERE c = 'a'::\"char\"") == "a")
check("D6 count", val("SELECT count(*) FROM t36_c") == "2")

# E. Comparisons are byte comparisons.
check("E1 eq", val("SELECT 'a'::\"char\" = 'a'::\"char\"") == "t")
check("E2 lt", val("SELECT 'a'::\"char\" < 'b'::\"char\"") == "t")
check("E3 high byte sorts after 'z'",
      val("SELECT '\\377'::\"char\" > 'z'::\"char\"") == "t")
check("E4 ne", val("SELECT 'a'::\"char\" <> 'b'::\"char\"") == "t")
do_sql("DROP TABLE IF EXISTS t36_o")
do_sql('CREATE TABLE t36_o (c "char")')
do_sql("INSERT INTO t36_o VALUES ('b'::\"char\"), ('\\377'::\"char\"), ('a'::\"char\")")
rows, _, _, _, _ = do_sql("SELECT c::text FROM t36_o ORDER BY c")
# v0.37: PG19 FigureColname names c::text as "c" (not "text"), so ORDER BY c
# resolves to the text output column per PG's "output column wins" rule.
# Text sort: "\\377" (backslash=92) < "a" < "b". The v0.36 expectation
# ["a","b","\\377"] matched the old incorrect "text" naming.
check("E5 order by byte", [r[0] for r in rows] == ["\\377", "a", "b"])

# F. Cast matrix (int4<->"char" are explicit per pg_cast.dat).
check("F1 int to char works", val("SELECT 65::\"char\"") == "A")
check("F2 char to int works", val("SELECT 'a'::\"char\"::int") == "97")
check("F3 bool to char is 42846", err_of("SELECT true::\"char\"") == "42846")

# G. Quoted-identifier behavior (never keywords, case-sensitive types).
check("G1 typed literal", val("SELECT \"char\" 'a'") == "a")
check("G2 function-style cast", val("SELECT \"char\"('a')") == "a")
check("G3 quoted alias may be reserved", val('SELECT 1 AS "select"') == "1")
_, _, _, cols, _ = do_sql('SELECT 1 AS "select"')
check("G4 quoted alias kept verbatim", cols == ["select"])
check("G5 quoted qualifier", val('SELECT "t36_o"."c"::text FROM "t36_o" LIMIT 1') in ("a", "b", "\\377"))
check("G6 \"CHAR\" is not a type", err_of("SELECT 'a'::\"CHAR\"") not in (None, "22001"))
check("G7 quoted keyword not a keyword", val('SELECT "count" FROM (SELECT 1 AS "count") q') == "1")

# H. pg_input_is_valid understands "char" (charin is total: all valid).
check("H1 valid", val("SELECT pg_input_is_valid('a', '\"char\"')") == "t")
check("H2 overlong is valid (first byte)", val("SELECT pg_input_is_valid('ab', '\"char\"')") == "t")
check("H3 octal valid", val("SELECT pg_input_is_valid('\\101', '\"char\"')") == "t")

do_sql("DROP TABLE t36_c")
do_sql("DROP TABLE t36_o")

print(f"protocol_test36: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
