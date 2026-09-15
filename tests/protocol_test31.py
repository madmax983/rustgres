#!/usr/bin/env python3
"""v0.31 protocol tests: SIMILAR TO fidelity (PG 19 similar_escape_internal port).

RED tests: SUBSTRING(.. SIMILAR .. ESCAPE ..) fails to parse (42601); ESCAPE ''
is wrongly rejected; escape-double-quote (#".."#) part separators, character
classes, and non-greedy part1 semantics are missing.
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
    # --- SUBSTRING .. SIMILAR .. ESCAPE: parser + escape-double-quote parts ---
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a#"(b_d)#"%' ESCAPE '#')""", [['bcd']], None),
    ("""SELECT SUBSTRING('abcdefg' FROM 'a#"(b_d)#"%' FOR '#')""", [['bcd']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR '#"(b_d)#"%' ESCAPE '#') IS NULL""", [['t']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR '%' ESCAPE NULL) IS NULL""", [['t']], None),
    ("""SELECT SUBSTRING(NULL SIMILAR '%' ESCAPE '#') IS NULL""", [['t']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR NULL ESCAPE '#') IS NULL""", [['t']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a#"%#"g' ESCAPE '#')""", [['bcdef']], None),
    # non-greedy part1: (a*){1,1}? matches empty, (.*) takes all
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a*#"%#"g*' ESCAPE '#')""", [['abcdefg']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a|b#"%#"g' ESCAPE '#')""", [['bcdef']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a#"%#"x|g' ESCAPE '#')""", [['bcdef']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a#"%|ab#"g' ESCAPE '#')""", [['bcdef']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a#"%g' ESCAPE '#')""", [['bcdefg']], None),
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a%g' ESCAPE '#')""", [['abcdefg']], None),
    # three escape-double-quotes is an error in PG
    ("""SELECT SUBSTRING('abcdefg' SIMILAR 'a*#"%#"g*#"x' ESCAPE '#')""", None, '2201B'),
    # --- SIMILAR TO: empty escape means no escape character ---
    (r"SELECT 'abcd\efg' SIMILAR TO '_bcd\%' ESCAPE ''", [['t']], None),
    (r"SELECT 'abcdefg' SIMILAR TO '_bcd\%' ESCAPE ''", [['f']], None),
    # --- SIMILAR TO: NULL escape -> NULL ---
    ("""SELECT 'abcdefg' SIMILAR TO '_bcd%' ESCAPE NULL""", [[None]], None),
    # --- SIMILAR TO: invalid escape string errors ---
    ("""SELECT 'abcdefg' SIMILAR TO '_bcd#%' ESCAPE '##'""", None, '22025'),
    # --- character classes: % and _ inside [...] are literal ---
    ("""SELECT 'a%b' SIMILAR TO 'a[%]b'""", [['t']], None),
    ("""SELECT 'a_b' SIMILAR TO 'a[_]b'""", [['t']], None),
    ("""SELECT 'axb' SIMILAR TO 'a[%]b'""", [['f']], None),
    # --- parens become non-capturing: no spurious group for SUBSTRING ---
    ("""SELECT SUBSTRING('abcdefg' SIMILAR '(a)#"%#"g' ESCAPE '#')""", [['bcdef']], None),
]

fails = 0
for sql, exp_rows, exp_err in TESTS:
    rows, err = do_sql(sql)
    if exp_err:
        if not err or err.get('C') != exp_err:
            print(f"FAIL (want err {exp_err}): {sql}\n  got rows={rows} err={err}")
            fails += 1
    else:
        if err or rows != exp_rows:
            print(f"FAIL: {sql}\n  want {exp_rows}, got rows={rows} err={err}")
            fails += 1
print(f"{len(TESTS)-fails}/{len(TESTS)} passed")
sys.exit(1 if fails else 0)
