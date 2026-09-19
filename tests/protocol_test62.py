#!/usr/bin/env python3
r"""v0.61 protocol tests: PG19 numeric power_var_int, display scale, gcd/lcm.

RED tests (all fail on v0.60, all pass on v0.61):
- PG19 power_var_int: exact integer-exponent power with adaptive working
  precision (src/backend/utils/adt/numeric.c, REL_19_STABLE). Ground truth
  is PG's own regression output in
  tests/conformance/data/expected/numeric.out.
- POSIX pow(3) specials: NaN ^ 0 = 1, 1 ^ NaN = 1.
- numeric_scale() returns the *display* scale (NUMERIC_DSCALE):
  scale('1.50') = 2, scale('-13.000000000000000') = 15.
- PG19 numeric_gcd/numeric_lcm: NaN/Infinity inputs -> NaN; the Euclidean
  algorithm runs over exact decimal values (fractionals included);
  display scale = max(dscale(a), dscale(b)).
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

# A. PG19 power_var_int exactness (expected values from numeric.out).
check("A1 3.789^21 dscale 16",
      val("select 3.789 ^ 21.0000000000000000") == "1409343026052.8716016316022141")
check("A2 3.789^35 dscale 16",
      val("select 3.789 ^ 35.0000000000000000") == "177158169650516670809.3820586142670135")
check("A3 1.2^345 rscale 1",
      val("select 1.2 ^ 345") == "2077446682327378559843444695.6")
check("A4 0.12^-20 rscale 2",
      val("select 0.12 ^ (-20)") == "2608405330458882702.55")
check("A5 0.12^-25 rscale 2",
      val("select 0.12 ^ (-25)") == "104825960103961013959336.50")
check("A6 0.5678^-85 rscale 4",
      val("select 0.5678 ^ (-85)") == "782333637740774446257.7719")
check("A7 tiny base huge negative exp",
      val("select 1.000000000123 ^ (-2147483648)") == "0.7678656556403084")
check("A8 underflow to zero at dscale 1000",
      val("select 10.0 ^ -2147483648") == "0." + "0" * 1000)
check("A9 underflow to zero at dscale 1000 (exp -2147483647)",
      val("select 10.0 ^ -2147483647") == "0." + "0" * 1000)

# B. POSIX pow(3) specials.
check("B1 NaN ^ 0 = 1", val("select 'NaN'::numeric ^ 0") == "1")
check("B2 1 ^ NaN = 1", val("select 1 ^ 'NaN'::numeric") == "1")

# C. scale() returns the display scale (NUMERIC_DSCALE).
check("C1 scale(1.50) = 2", val("select scale(1.50)") == "2")
check("C2 scale(0.00) = 2", val("select scale(0.00)") == "2")
check("C3 scale(-13.000000000000000) = 15",
      val("select scale(-13.000000000000000)") == "15")

# D. Display scale is retained on output.
check("D1 select 21.00 keeps dscale", val("select 21.00") == "21.00")
check("D2 select 21.0000000000000000 keeps dscale",
      val("select 21.0000000000000000") == "21.0000000000000000")

# E. PG19 numeric gcd/lcm (expected rows from numeric.out).
GCD_Q = ("SELECT a, b, gcd(a, b), gcd(a, -b), gcd(-b, a), gcd(-b, -a) "
         "FROM (VALUES (0::numeric, 0::numeric), (0::numeric, numeric 'NaN'), "
         "(0::numeric, 46375::numeric), (433125::numeric, 46375::numeric), "
         "(43312.5::numeric, 4637.5::numeric), "
         "(4331.250::numeric, 463.75000::numeric), "
         "('inf', '0'), ('inf', '42'), ('inf', 'inf')) AS v(a, b)")
rows, err = do_sql(GCD_Q)
check("E1 gcd table succeeds", err is None)
if err is None:
    check("E2 gcd table rows", rows == [
        ["0", "0", "0", "0", "0", "0"],
        ["0", "NaN", "NaN", "NaN", "NaN", "NaN"],
        ["0", "46375", "46375", "46375", "46375", "46375"],
        ["433125", "46375", "875", "875", "875", "875"],
        ["43312.5", "4637.5", "87.5", "87.5", "87.5", "87.5"],
        ["4331.250", "463.75000", "8.75000", "8.75000", "8.75000", "8.75000"],
        ["Infinity", "0", "NaN", "NaN", "NaN", "NaN"],
        ["Infinity", "42", "NaN", "NaN", "NaN", "NaN"],
        ["Infinity", "Infinity", "NaN", "NaN", "NaN", "NaN"],
    ])

LCM_Q = ("SELECT a, b, lcm(a, b), lcm(a, -b), lcm(-b, a), lcm(-b, -a) "
         "FROM (VALUES (0::numeric, 0::numeric), (0::numeric, numeric 'NaN'), "
         "(0::numeric, 13272::numeric), (13272::numeric, 13272::numeric), "
         "(423282::numeric, 13272::numeric), "
         "(42328.2::numeric, 1327.2::numeric), "
         "(4232.820::numeric, 132.72000::numeric), "
         "('inf', '0'), ('inf', '42'), ('inf', 'inf')) AS v(a, b)")
rows, err = do_sql(LCM_Q)
check("E3 lcm table succeeds", err is None)
if err is None:
    check("E4 lcm table rows", rows == [
        ["0", "0", "0", "0", "0", "0"],
        ["0", "NaN", "NaN", "NaN", "NaN", "NaN"],
        ["0", "13272", "0", "0", "0", "0"],
        ["13272", "13272", "13272", "13272", "13272", "13272"],
        ["423282", "13272", "11851896", "11851896", "11851896", "11851896"],
        ["42328.2", "1327.2", "1185189.6", "1185189.6", "1185189.6", "1185189.6"],
        ["4232.820", "132.72000", "118518.96000", "118518.96000",
         "118518.96000", "118518.96000"],
        ["Infinity", "0", "NaN", "NaN", "NaN", "NaN"],
        ["Infinity", "42", "NaN", "NaN", "NaN", "NaN"],
        ["Infinity", "Infinity", "NaN", "NaN", "NaN", "NaN"],
    ])

print(f"protocol_test62: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
