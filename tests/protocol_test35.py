#!/usr/bin/env python3
r"""v0.35 protocol tests: char(n)/varchar(n) typmod enforcement (PG 19 semantics).

RED tests: PG 19's varchar.c (REL_19_STABLE) rules:
- char(n) pads to exactly n chars; varchar(n) does not pad.
- Overlength assignment/input: excess must be all spaces (silently
  clipped), else SQLSTATE 22001 "value too long for type character(n)".
- Explicit casts (CAST/x::type) silently truncate ANY excess.
- char without length = char(1); varchar without length = unlimited.
- Lengths count Unicode characters, not bytes.
- bpchar comparisons ignore trailing spaces; length(bpchar) ignores
  trailing spaces (bpchartruelen); octet_length(bpchar) counts padded bytes.
- bpchar -> text/varchar casts strip trailing spaces (rtrim1).
- Concatenation coerces bpchar to text first (trailing spaces stripped).
- Typmod 0 or negative -> 22023.
- pg_input_is_valid() understands char(n)/varchar(n) typmods.

Ground truth: PG REL_19_STABLE src/backend/utils/adt/varchar.c and
src/test/regress/expected/{char,varchar}.out.
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
    rows, err, cols = [], None, []
    while True:
        t, p = read_msg(s)
        if t == b'T':
            (n,) = struct.unpack("!h", p[:2]); pos = 2
            for _ in range(n):
                e = p.index(b'\x00', pos); cols.append(p[pos:e].decode()); pos = e + 1
                (oid,) = struct.unpack("!i", p[pos+6:pos+10]); pos += 18
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
            err = f.get('C')
        elif t == b'Z':
            break
    return rows, err, cols

def val(q):
    rows, err, _ = do_sql(q)
    assert not err, f"{q} -> SQLSTATE {err}"
    assert len(rows) == 1 and len(rows[0]) == 1, f"{q} -> {rows}"
    return rows[0][0]

def err_of(q):
    _, err, _ = do_sql(q)
    return err

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

# A. Explicit CAST to char(n): silent truncation, blank padding.
check("A1 cast truncates", val("SELECT CAST('abcdef' AS char(3))") == "abc")
check("A2 cast :: truncates", val("SELECT 'abcdef'::char(3)") == "abc")
check("A3 cast pads", val("SELECT CAST('ab' AS char(5))") == "ab   ")
check("A4 cast truncates long", val("SELECT CAST('hi de ho neighbor' AS char(10))") == "hi de ho n")
check("A5 bare char is char(1)", val("SELECT CAST('abcdef' AS char)") == "a")
check("A6 bare char pads", val("SELECT CAST('' AS char)") == " ")

# B. Explicit CAST to varchar(n): silent truncation, no padding.
check("B1 varchar cast truncates", val("SELECT CAST('abcdef' AS varchar(3))") == "abc")
check("B2 varchar cast no pad", val("SELECT CAST('ab' AS varchar(5))") == "ab")
check("B3 bare varchar unlimited", val("SELECT CAST('abcdef' AS varchar)") == "abcdef")

# C. Typmod validation.
check("C1 char(0) is 22023", err_of("SELECT CAST('x' AS char(0))") == "22023")
check("C2 varchar(0) is 22023", err_of("SELECT CAST('x' AS varchar(0))") == "22023")

# D. Table column definitions + INSERT assignment semantics.
do_sql("DROP TABLE IF EXISTS t35_char")
do_sql("DROP TABLE IF EXISTS t35_varchar")
do_sql("DROP TABLE IF EXISTS t35_c1")
do_sql("DROP TABLE IF EXISTS t35_vu")
do_sql("DROP TABLE IF EXISTS t35_u")
check("D1 create char(3)", err_of("CREATE TABLE t35_char (c char(3))") is None)
check("D2 insert overlength is 22001",
      err_of("INSERT INTO t35_char VALUES ('abcdef')") == "22001")
check("D3 insert ok", err_of("INSERT INTO t35_char VALUES ('ab')") is None)
check("D4 select shows padded",
      val("SELECT c FROM t35_char") == "ab ")
check("D5 insert trailing blanks ok",
      err_of("INSERT INTO t35_char VALUES ('ab    ')") is None)
check("D6 insert nonblank excess is 22001",
      err_of("INSERT INTO t35_char VALUES ('abcd  ')") == "22001")
check("D7 row count", val("SELECT count(*) FROM t35_char") == "2")
do_sql("CREATE TABLE t35_varchar (v varchar(3))")
check("D8 varchar insert overlength is 22001",
      err_of("INSERT INTO t35_varchar VALUES ('abcdef')") == "22001")
check("D9 varchar insert ok", err_of("INSERT INTO t35_varchar VALUES ('ab')") is None)
check("D10 varchar select not padded",
      val("SELECT v FROM t35_varchar") == "ab")
check("D11 varchar insert trailing blanks ok",
      err_of("INSERT INTO t35_varchar VALUES ('ab    ')") is None)
check("D12 varchar trailing blanks clipped to typmod",
      val("SELECT v FROM t35_varchar WHERE v = 'ab '") == "ab ")
do_sql("CREATE TABLE t35_c1 (c char)")
check("D13 bare char col is char(1)",
      err_of("INSERT INTO t35_c1 VALUES ('ab')") == "22001")
check("D14 bare char col ok", err_of("INSERT INTO t35_c1 VALUES ('a')") is None)
do_sql("CREATE TABLE t35_vu (v varchar)")
check("D15 bare varchar unlimited",
      err_of("INSERT INTO t35_vu VALUES ('abcdefghijklmnopqrstuvwxyz')") is None)

# E. bpchar comparisons ignore trailing spaces.
check("E1 padded = unpadded",
      val("SELECT c = 'ab' FROM t35_char WHERE c = 'ab ' LIMIT 1") == "t")
check("E2 padded = padded", val("SELECT c = 'ab ' FROM t35_char WHERE c = 'ab' LIMIT 1") == "t")
check("E3 cast cmp", val("SELECT 'ab'::char(3) = 'ab  '") == "t")
check("E4 cast cmp lt", val("SELECT CAST('b' AS char(3)) > CAST('a' AS char(3))") == "t")
check("E5 varchar cmp exact", val("SELECT 'ab'::varchar(3) = 'ab '") == "f")

# F. char -> text/varchar casts strip trailing spaces.
check("F1 char->text rtrims", val("SELECT CAST(CAST('ab' AS char(5)) AS text)") == "ab")
check("F2 char->varchar rtrims", val("SELECT CAST(CAST('ab' AS char(5)) AS varchar)") == "ab")
check("F3 char->varchar(n) rtrims",
      val("SELECT CAST(CAST('ab' AS char(5)) AS varchar(10))") == "ab")

# G. length/octet_length on bpchar.
check("G1 length ignores trailing spaces",
      val("SELECT length(CAST('ab' AS char(5)))") == "2")
check("G2 octet_length counts padded bytes",
      val("SELECT octet_length(CAST('ab' AS char(5)))") == "5")

# H. Concatenation coerces bpchar to text (trailing spaces stripped).
check("H1 concat rtrims char", val("SELECT CAST('ab' AS char(5)) || 'cd'") == "abcd")
check("H2 concat rtrims both",
      val("SELECT CAST('ab' AS char(5)) || CAST('cd' AS char(5))") == "abcd")
check("H3 insert concat coerced",
      err_of("INSERT INTO t35_varchar VALUES (CAST('ab' AS char(5)) || 'cdef')") == "22001")

# I. UPDATE assignment enforces typmod too.
check("I1 update overlength is 22001",
      err_of("UPDATE t35_char SET c = 'abcdef'") == "22001")
check("I2 update ok", err_of("UPDATE t35_char SET c = 'xy' WHERE c = 'ab'") is None)
check("I3 update stored padded",
      val("SELECT c FROM t35_char WHERE c = 'xy' LIMIT 1") == "xy ")

# J. Unicode: lengths count characters, not bytes.
check("J1 unicode cast truncates by chars",
      val("SELECT CAST('héllo' AS char(4))") == "héll")
check("J2 unicode cast pads by chars",
      val("SELECT octet_length(CAST('h' AS char(2)))") == "2")
do_sql("CREATE TABLE t35_u (c char(2))")
check("J3 unicode insert overlength is 22001",
      err_of("INSERT INTO t35_u VALUES ('héllo')") == "22001")
check("J4 unicode insert ok", err_of("INSERT INTO t35_u VALUES ('hé')") is None)
check("J5 unicode length", val("SELECT length(c) FROM t35_u") == "2")

# K. pg_input_is_valid understands char(n)/varchar(n).
check("K1 valid blanks", val("SELECT pg_input_is_valid('abcd  ', 'char(4)')") == "t")
check("K2 invalid overlength", val("SELECT pg_input_is_valid('abcde', 'char(4)')") == "f")
check("K3 invalid varchar", val("SELECT pg_input_is_valid('abcde', 'varchar(4)')") == "f")
check("K4 valid varchar", val("SELECT pg_input_is_valid('abcd', 'varchar(4)')") == "t")
check("K5 LIKE on bpchar coerces to text",
      val("SELECT 'ab   '::char(5) LIKE 'ab%'") == "t")
check("K6 ILIKE on bpchar coerces to text",
      val("SELECT 'AB   '::char(5) ILIKE 'ab%'") == "t")
check("K7 LIKE with bpchar pattern",
      val("SELECT 'abc' LIKE CAST('ab%' AS char(5))") == "t")

# L. Expression assignment: INSERT of a too-long expression errors.
check("L1 expr insert overlength is 22001",
      err_of("INSERT INTO t35_char VALUES ('ab' || 'cd')") == "22001")

# M. Column type names on the wire: bpchar/varchar.
_, _, cols = do_sql("SELECT CAST('ab' AS char(3))")
check("M1 cast char col named bpchar", cols == ["bpchar"])
_, _, cols = do_sql("SELECT CAST('ab' AS varchar(3))")
check("M2 cast varchar col named varchar", cols == ["varchar"])

do_sql("DROP TABLE t35_char")
do_sql("DROP TABLE t35_varchar")
do_sql("DROP TABLE t35_c1")
do_sql("DROP TABLE t35_vu")
do_sql("DROP TABLE t35_u")

print(f"protocol_test35: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
