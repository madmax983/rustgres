#!/usr/bin/env python3
"""v0.33 protocol tests: bytea string functions (PG 19 semantics).

RED tests: SUBSTRING(bytea FROM int), SUBSTRING(bytea FROM int FOR int),
trim(bytea FROM bytea) SQL syntax, btrim(bytea, bytea), ltrim/rtrim(bytea, bytea),
and overlay(bytea PLACING bytea FROM int [FOR int]) all return 42883
(undefined function).

Ground truth: PG REL_19_STABLE
- src/backend/utils/adt/bytea.c (bytea_substring, bytea_overlay,
  bytea_substr, bytea_substr_no_len, byteaoverlay, byteaoverlay_no_len)
- src/backend/utils/adt/oracle_compat.c (dobyteatrim, byteatrim,
  bytealtrim, byteartrim)
- src/include/catalog/pg_proc.dat (signatures)
- src/test/regress/expected/strings.out (expected outputs)

Bytea results are verified via encode(x, 'hex') for exact byte comparison,
avoiding the escape-format rendering question.
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
            fields, pos = {}, 0
            while p[pos] != 0:
                code = chr(p[pos]); pos += 1
                end = p.index(b'\x00', pos)
                fields[code] = p[pos:end].decode(); pos = end + 1
            err = fields
        elif t == b'Z':
            break
    return rows, err

# (sql, expected_rows_or_None, expected_error_code_or_None)
TESTS = [
    # --- substring(bytea): SQL syntax ---
    ("""SELECT encode(SUBSTRING('1234567890'::bytea FROM 3), 'hex')""",
     [['3334353637383930']], None),
    ("""SELECT encode(SUBSTRING('1234567890'::bytea FROM 4 FOR 3), 'hex')""",
     [['343536']], None),
    ("""SELECT encode(SUBSTRING('string'::bytea FROM 2 FOR 2147483646), 'hex')""",
     [['7472696e67']], None),
    # start before 1 with huge length: whole string (PG bytea_substring)
    ("""SELECT encode(SUBSTRING('string'::bytea FROM -10 FOR 2147483646), 'hex')""",
     [['737472696e67']], None),
    # start past end -> empty
    ("""SELECT encode(SUBSTRING('string'::bytea FROM 100), 'hex')""",
     [['']], None),
    # zero length -> empty
    ("""SELECT encode(SUBSTRING('string'::bytea FROM 2 FOR 0), 'hex')""",
     [['']], None),
    # negative length -> 22011
    ("""SELECT SUBSTRING('string'::bytea FROM 2 FOR -1)""",
     None, '22011'),
    # --- substring(bytea): function form ---
    ("""SELECT encode(substring('1234567890'::bytea, 3), 'hex')""",
     [['3334353637383930']], None),
    ("""SELECT encode(substring('1234567890'::bytea, 4, 3), 'hex')""",
     [['343536']], None),
    ("""SELECT encode(substr('1234567890'::bytea, 4, 3), 'hex')""",
     [['343536']], None),
    ("""SELECT substring('string'::bytea, 2, -1)""",
     None, '22011'),
    # NULL propagation
    ("""SELECT substring(NULL::bytea, 1)""",
     [[None]], None),
    ("""SELECT substring('abc'::bytea, NULL)""",
     [[None]], None),
    # --- trim(bytea): SQL syntax -> btrim/ltrim/rtrim ---
    ("""SELECT encode(trim(E'\\000'::bytea from E'\\000Tom\\000'::bytea), 'hex')""",
     [['546f6d']], None),
    ("""SELECT encode(trim(leading E'\\000'::bytea from E'\\000Tom\\000'::bytea), 'hex')""",
     [['546f6d00']], None),
    ("""SELECT encode(trim(trailing E'\\000'::bytea from E'\\000Tom\\000'::bytea), 'hex')""",
     [['00546f6d']], None),
    ("""SELECT encode(trim(both E'\\000'::bytea from E'\\000Tom\\000'::bytea), 'hex')""",
     [['546f6d']], None),
    # --- btrim/ltrim/rtrim(bytea, bytea): function form ---
    ("""SELECT encode(btrim(E'\\000trim\\000'::bytea, E'\\000'::bytea), 'hex')""",
     [['7472696d']], None),
    ("""SELECT encode(btrim(''::bytea, E'\\000'::bytea), 'hex')""",
     [['']], None),
    # empty set -> string unchanged (PG dobyteatrim)
    ("""SELECT encode(btrim(E'\\000trim\\000'::bytea, ''::bytea), 'hex')""",
     [['007472696d00']], None),
    ("""SELECT encode(ltrim(E'\\000trim\\000'::bytea, E'\\000'::bytea), 'hex')""",
     [['7472696d00']], None),
    ("""SELECT encode(rtrim(E'\\000trim\\000'::bytea, E'\\000'::bytea), 'hex')""",
     [['007472696d']], None),
    # NULL propagation
    ("""SELECT btrim(NULL::bytea, E'\\000'::bytea)""",
     [[None]], None),
    # --- overlay(bytea): SQL syntax ---
    ("""SELECT encode(overlay(E'Th\\000omas'::bytea placing E'Th\\001omas'::bytea from 2), 'hex')""",
     [['545468016f6d6173']], None),
    ("""SELECT encode(overlay(E'Th\\000omas'::bytea placing E'\\002\\003'::bytea from 8), 'hex')""",
     [['5468006f6d61730203']], None),
    ("""SELECT encode(overlay(E'Th\\000omas'::bytea placing E'\\002\\003'::bytea from 5 for 3), 'hex')""",
     [['5468006f0203']], None),
    # text overlay still works (regression)
    ("""SELECT overlay('1234567890' placing 'abc' from 2 for 3)""",
     [['1abc567890']], None),
    # sp <= 0 -> 22011 (PG bytea_overlay errors, unlike text overlay)
    ("""SELECT overlay('abc'::bytea placing 'x'::bytea from 0)""",
     None, '22011'),
    ("""SELECT overlay('abc'::bytea placing 'x'::bytea from -1)""",
     None, '22011'),
    # negative length -> 22011
    ("""SELECT overlay('abc'::bytea placing 'x'::bytea from 2 for -1)""",
     None, '22011'),
    # NULL propagation
    ("""SELECT overlay(NULL::bytea placing 'x'::bytea from 2)""",
     [[None]], None),
    # --- position(bytea): regression (already implemented) ---
    ("""SELECT position(E'\\000'::bytea in E'\\000Tom\\000'::bytea)""",
     [['1']], None),
]

fails = 0
for sql, exp_rows, exp_err in TESTS:
    rows, err = do_sql(sql)
    if exp_err:
        if not err or err.get('C') != exp_err:
            print(f"FAIL (want error {exp_err}): {sql}\n  got rows={rows} err={err}")
            fails += 1
    else:
        if err or rows != exp_rows:
            print(f"FAIL: {sql}\n  want {exp_rows}\n  got  rows={rows} err={err}")
            fails += 1
print(f"{len(TESTS)-fails}/{len(TESTS)} passed")
sys.exit(1 if fails else 0)
