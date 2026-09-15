#!/usr/bin/env python3
"""v0.30 protocol tests: base64url fidelity + crc32/crc32c (PG 18/19).

RED tests: base64url currently pads on encode and rejects unpadded input on
decode. PG 19 uses unpadded base64url output and accepts unpadded (or
optionally padded) input. crc32/crc32c are new in PG 18.
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

# (sql, expected_rows_or_None, expected_error_code_or_None, expected_error_substring)
TESTS = [
    # --- base64url encode: no padding ---
    ("SELECT encode('\\x01', 'base64url')", [['AQ']], None, None),
    ("SELECT encode('\\x69b73eff', 'base64url')", [['abc-_w']], None, None),
    ("SELECT encode('\\x0102'::bytea, 'base64url')", [['AQI']], None, None),
    ("SELECT encode('\\xdeadbeef'::bytea, 'base64url')", [['3q2-7w']], None, None),
    ("SELECT encode('', 'base64url')", [['']], None, None),
    # --- base64url decode: accept unpadded ---
    ("SELECT decode('AQ', 'base64url')", [['\\x01']], None, None),
    ("SELECT decode('abc-_w', 'base64url')", [['\\x69b73eff']], None, None),
    ("SELECT decode('QQ', 'base64url')", [['\\x41']], None, None),
    ("SELECT decode('QQI', 'base64url')", [['\\x4102']], None, None),
    # --- base64url decode: optional padding accepted ---
    ("SELECT decode('abc-_w==', 'base64url')", [['\\x69b73eff']], None, None),
    # --- base64url decode: specific errors ---
    ("SELECT decode('QQIDQ', 'base64url')", None, '22023', 'invalid base64url end sequence'),
    ("SELECT decode('=QQQ', 'base64url')", None, '22023', 'unexpected "=" while decoding base64url sequence'),
    ("SELECT decode('QQ@=', 'base64url')", None, '22023', 'invalid symbol "@" found while decoding base64url sequence'),
    # --- base64url round-trip ---
    ("SELECT encode(decode('abc-_w', 'base64url'), 'base64url')", [['abc-_w']], None, None),
    # --- crc32 (PG 18+) ---
    ("SELECT crc32('hello world'::bytea)", [['222957957']], None, None),
    ("SELECT crc32(''::bytea)", [['0']], None, None),
    # --- crc32c (PG 18+) ---
    ("SELECT crc32c('hello world'::bytea)", None, None, None),  # value checked loosely below
]

fails = 0
for q, exp_rows, exp_code, exp_sub in TESTS:
    rows, err = do_sql(q)
    if exp_code:
        if not err or err.get('C') != exp_code:
            print(f"FAIL {q}\n  expected error {exp_code}, got {err}")
            fails += 1
        elif exp_sub and exp_sub not in err.get('M', ''):
            print(f"FAIL {q}\n  error message missing {exp_sub!r}: {err.get('M')}")
            fails += 1
        else:
            print(f"ok   {q}")
    elif exp_rows is None:
        # loose check: just needs to succeed and return one row
        if err or len(rows) != 1:
            print(f"FAIL {q}\n  expected success, got err={err} rows={rows}")
            fails += 1
        else:
            print(f"ok   {q} -> {rows[0][0]}")
    else:
        if err:
            print(f"FAIL {q}\n  unexpected error {err.get('C')}: {err.get('M')}")
            fails += 1
        elif rows != exp_rows:
            print(f"FAIL {q}\n  expected {exp_rows}, got {rows}")
            fails += 1
        else:
            print(f"ok   {q}")

s.close()
print(f"\n{len(TESTS) - fails}/{len(TESTS)} passed")
sys.exit(1 if fails else 0)
