#!/usr/bin/env python3
r"""v0.34 protocol tests: regexp_replace fidelity (PG 19 semantics).

RED tests: PG 19's replace_text_regexp rules from
src/backend/utils/adt/varlena.c (REL_19_STABLE):
- n (count): "if 0, replace all matches; if > 0, replace only the N'th match"
- If N not specified, n = 'g' in flags ? 0 : 1  (default is FIRST match only)
- start is 1-based; search_start = start - 1 (0-based); start <= 0 errors
- Zero-width matches advance search by one char but the copy cursor
  (data_pos) only moves over replaced/copied text
- Replacement escapes: \\ -> backslash, \1..\9 -> groups, \& -> whole match,
  other \x -> literal backslash + x, trailing \ -> literal backslash

Ground truth: PG REL_19_STABLE src/backend/utils/adt/varlena.c
(replace_text_regexp) and src/test/regress/expected/strings.out.
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
    rows, err = [], None
    while True:
        t, p = read_msg(s)
        if t == b'D':
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
    return rows, err

def val(q):
    rows, err = do_sql(q)
    assert not err, f"{q} -> SQLSTATE {err}"
    assert len(rows) == 1 and len(rows[0]) == 1, f"{q} -> {rows}"
    return rows[0][0]

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

# A. n defaults to 1 (first match only) when N not given and no 'g'.
check("A1 no-g replaces first only",
      val(r"SELECT regexp_replace('foobarrbazz', E'(.)\\1', E'X\\Y\\1Z\\')") == "fX\\YoZ\\barrbazz")
check("A2 no-g 3-arg replaces first only",
      val(r"SELECT regexp_replace('1112223333', E'(\\d{3})(\\d{3})(\\d{4})', E'(\\1) \\2-\\3')") == "(111) 222-3333")

# B. 'g' flag replaces all.
check("B1 g replaces all",
      val(r"SELECT regexp_replace('foobarrbazz', E'(.)\\1', E'X\\&Y', 'g')") == "fXooYbaXrrYbaXzzY")

# C. Integer 4th arg is START (1-based); n still defaults to 1.
check("C1 start=1 int form replaces first",
      val(r"SELECT regexp_replace('A PostgreSQL function', 'A|e|i|o|u', 'X', 1)") == "X PostgreSQL function")
check("C2 start skips earlier text",
      val(r"SELECT regexp_replace('A PostgreSQL function', 'A|e|i|o|u', 'X', 7, 0, 'i')") == "A PostgrXSQL fXnctXXn")

# D. Explicit n replaces only the N'th match; 'g' ignored when n given.
check("D1 n=2 replaces second only",
      val(r"SELECT regexp_replace('A PostgreSQL function', 'A|e|i|o|u', 'X', 1, 2)") == "A PXstgreSQL function")
check("D2 n=0 replaces all",
      val(r"SELECT regexp_replace('A PostgreSQL function', 'a|e|i|o|u', 'X', 1, 0, 'i')") == "X PXstgrXSQL fXnctXXn")
check("D3 n=9 beyond matches is no-op",
      val(r"SELECT regexp_replace('A PostgreSQL function', 'a|e|i|o|u', 'X', 1, 9, 'i')") == "A PostgreSQL function")
check("D4 g ignored when n specified",
      val(r"SELECT regexp_replace('A PostgreSQL function', 'a|e|i|o|u', 'X', 1, 1, 'g')") == "A PXstgreSQL function")

# E. Zero-width matches: copy cursor vs search cursor.
check("E1 ^|$ g inserts at both ends",
      val(r"SELECT regexp_replace('AAA', '^|$', 'Z', 'g')") == "ZAAAZ")
check("E2 zero-width global keeps text",
      val(r"SELECT regexp_replace('ab', 'x*', 'Z', 'g')") == "ZaZbZ")

# F. Replacement-string escapes (PG: \\ -> \, \1..\9 -> groups, \& -> whole,
# other \x -> literal backslash + x).
check("F1 backslash escapes in replacement",
      val(r"SELECT regexp_replace('foobarrbazz', E'(.)\\1', E'X\\\\\\\\Y', 'g')") == "fX\\\\YbaX\\\\YbaX\\\\Y")

print(f"{passed}/{passed+failed} passed")
sys.exit(1 if failed else 0)
