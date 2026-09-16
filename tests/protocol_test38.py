#!/usr/bin/env python3
r"""v0.38 protocol tests: PG19 TOAST biggest-first planning, regclass/oid
comparison coercion, regexp_replace \0, and the conformance-harness fixes.

RED (base 3c66c32, before this milestone):
- regexp_replace('abc','b','X\0Y') -> 'aXbYc' (treated \0 as whole-match).
- SELECT ... WHERE oid = 't'::regclass -> 22P02 (or empty); the regclass
  display text was parsed as an integer OID.
- Two EXTERNAL columns (4000 + 120 bytes, target 128): toast relation had
  ONE chunk_seq=0 row (one value externalized, split into 2 chunks) --
  the fit calculation ignored already-externalized values.
- strings conformance: 5 REAL-FAIL (2 TRIM header, 2 :reltoastname from
  missing \gset support, 1 base64 multiline).

GREEN (this milestone):
- regexp_replace: only \1..\9 are backreferences; \0 is literal (PG19
  varlena.c: unknown escapes keep the backslash). \&, \\, unknown \q,
  and trailing \ behave per PG.
- oid = 'name'::regclass compares numeric OIDs both directions.
- TOAST follows PG19 biggest-first: two EXTERNAL values -> two
  chunk_seq=0 rows; MAIN compressible columns stay inline with
  pg_column_compression='pglz'; huge EXTENDED value -> one external row.
- strings conformance: 0 REAL-FAIL (harness: trailing \gset + :var
  interpolation, psql '+' newline-marker stripping, stripped colnames).

Honest non-claims (NOT tested as working): pglz is a custom LZ77, not
PGLZ byte-compatible; pg_relation_size is an in-memory estimate; TOAST
WAL/chunk durability across restarts and DELETE/VACUUM chunk cleanup are
incomplete; tuple-size accounting sums toastable payloads, not the full
aligned tuple.
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
    rows, err, msg, cols = [], None, None, []
    while True:
        t, p = read_msg(s)
        if t == b'T':
            (n,) = struct.unpack("!h", p[:2]); pos = 2
            for _ in range(n):
                e = p.index(b'\x00', pos); cols.append(p[pos:e].decode()); pos = e + 1
                pos += 18
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
    return rows, err, msg, cols

def val(q):
    rows, err, msg, _ = do_sql(q)
    assert not err, f"{q} -> SQLSTATE {err}: {msg}"
    assert len(rows) == 1 and len(rows[0]) == 1, f"{q} -> {rows}"
    return rows[0][0]

def err_of(q):
    _, err, _, _ = do_sql(q)
    return err

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

# A. regexp_replace \0 is literal (PG19 varlena.c).
check("A1 backslash-zero literal",
      val(r"SELECT regexp_replace('abc', 'b', 'X\0Y')") == r"aX\0Yc")
check("A2 backslash-zero alone",
      val(r"SELECT regexp_replace('abc', 'b', '\0')") == r"a\0c")
# B. Other escapes per PG19.
check("B1 group ref", val(r"SELECT regexp_replace('abc', '(b)', 'X\1Y')") == "aXbYc")
check("B2 whole match", val(r"SELECT regexp_replace('abc', 'b', 'X\&Y')") == "aXbYc")
check("B3 double backslash", val(r"SELECT regexp_replace('abc', 'b', 'X\\Y')") == r"aX\Yc")
check("B4 unknown escape kept", val(r"SELECT regexp_replace('abc', 'b', 'X\qY')") == r"aX\qYc")
check("B5 trailing backslash",
      val("SELECT regexp_replace('abc', 'b', 'X\\')") == "aX\\c")
check("B6 group 9", val(r"SELECT regexp_replace('abcdefghi', '(a)(b)(c)(d)(e)(f)(g)(h)(i)', '\9')") == "i")
# C. Match iteration per PG19 (guards; C3's empty-match full iteration is a
# known pre-existing gap, RED on base and unchanged by v0.38, so not claimed).
check("C1 nth match", val(r"SELECT regexp_replace('aaa', 'a', 'b', 1, 2)") == "aba")
check("C2 all matches", val(r"SELECT regexp_replace('aaa', 'a', 'b', 1, 0)") == "bbb")

# D. regclass/oid comparison coercion.
do_sql("DROP TABLE IF EXISTS t38reg")
do_sql("CREATE TABLE t38reg(a int)")
d1_rows, d1_err, _, _ = do_sql("SELECT count(*) FROM pg_class WHERE oid = 't38reg'::regclass")
check("D1 regclass = oid", d1_err is None and d1_rows == [["1"]])
d2_rows, d2_err, _, _ = do_sql("SELECT count(*) FROM pg_class WHERE 't38reg'::regclass = oid")
check("D2 oid = regclass (reversed)", d2_err is None and d2_rows == [["1"]])
d3_rows, d3_err, _, _ = do_sql("SELECT relname FROM pg_class WHERE oid = 't38reg'::regclass")
check("D3 regclass relname", d3_err is None and d3_rows == [["t38reg"]])
check("D4 unknown name errors",
      err_of("SELECT 'nosuchtable_xyz'::regclass") == "42P01")

# E. TOAST: two EXTERNAL values, target 128 (PG19 biggest-first).
do_sql("DROP TABLE IF EXISTS t38toast")
do_sql("CREATE TABLE t38toast(a text, b text)")
do_sql("ALTER TABLE t38toast ALTER COLUMN a SET STORAGE external")
do_sql("ALTER TABLE t38toast ALTER COLUMN b SET STORAGE external")
do_sql("ALTER TABLE t38toast SET (toast_tuple_target = 128)")
do_sql("INSERT INTO t38toast VALUES (repeat('1', 4000), repeat('2', 120))")
toastrel = val("SELECT relname FROM pg_class WHERE oid = "
               "(SELECT reltoastrelid FROM pg_class WHERE relname = 't38toast')")
check("E1 toast rel exists", toastrel.startswith("pg_toast."))
check("E2 two chunk_seq=0 rows",
      val(f"SELECT count(*) FROM {toastrel} WHERE chunk_seq = 0") == "2")
# PG chunks large values (TOAST_MAX_CHUNK_SIZE): the 4000-byte value spans
# 2 chunks, the 120-byte value is a single chunk.
check("E3 big value chunked",
      val(f"SELECT count(*) FROM {toastrel} WHERE chunk_seq = 1") == "1")
check("E3b three rows total",
      val(f"SELECT count(*) FROM {toastrel}") == "3")
check("E4 external not compressed",
      val("SELECT pg_column_compression(a) FROM t38toast") is None)
check("E5 roundtrip",
      val("SELECT length(a) || '/' || length(b) FROM t38toast") == "4000/120")

# F. TOAST: MAIN compressible columns stay inline.
do_sql("DROP TABLE IF EXISTS t38main")
do_sql("CREATE TABLE t38main(a text, b text)")
do_sql("ALTER TABLE t38main ALTER COLUMN a SET STORAGE main")
do_sql("ALTER TABLE t38main ALTER COLUMN b SET STORAGE main")
do_sql("ALTER TABLE t38main SET (toast_tuple_target = 128)")
do_sql("INSERT INTO t38main VALUES (repeat('3', 4000), repeat('4', 4000))")
mrel = val("SELECT relname FROM pg_class WHERE oid = "
           "(SELECT reltoastrelid FROM pg_class WHERE relname = 't38main')")
check("F1 main inline zero toast rows",
      val(f"SELECT count(*) FROM {mrel}") == "0")
check("F2 main compressed a",
      val("SELECT pg_column_compression(a) FROM t38main") == "pglz")
check("F3 main compressed b",
      val("SELECT pg_column_compression(b) FROM t38main") == "pglz")
check("F4 main roundtrip",
      val("SELECT length(a) || '/' || length(b) FROM t38main") == "4000/4000")

# G. TOAST: one huge EXTENDED value -> single external row.
do_sql("DROP TABLE IF EXISTS t38ext")
do_sql("CREATE TABLE t38ext(a text)")
do_sql("ALTER TABLE t38ext SET (toast_tuple_target = 128)")
do_sql("INSERT INTO t38ext VALUES (repeat('5', 100000))")
erel = val("SELECT relname FROM pg_class WHERE oid = "
           "(SELECT reltoastrelid FROM pg_class WHERE relname = 't38ext')")
check("G1 one chunk_seq=0 row",
      val(f"SELECT count(*) FROM {erel} WHERE chunk_seq = 0") == "1")
check("G2 extended compressed",
      val("SELECT pg_column_compression(a) FROM t38ext") == "pglz")
check("G3 extended roundtrip", val("SELECT length(a) FROM t38ext") == "100000")

do_sql("DROP TABLE t38reg")
do_sql("DROP TABLE t38toast")
do_sql("DROP TABLE t38main")
do_sql("DROP TABLE t38ext")

print(f"{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
