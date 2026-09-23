#!/usr/bin/env python3
r"""v0.63 protocol tests: arbitrary-precision numeric (big-mantissa extension).

RED on the inherited base (097bef6: v0.62 + Bolt), GREEN after v0.63.

v0.62 honestly returned 22003 past the i128 wall (~38 digits). v0.63
extends `Numeric` with a big-magnitude (`BigUint`) so exact decimal
results hold up to PG19's numeric limits (131072 digits before the
point, 16383 after). Ground truth is PG's own regression output in
tests/conformance/data/expected/numeric.out.

- Exact big literals and big add/mul/div.
- Integer power past the i128 wall: `1.234 ^ 5678` (523-char exact).
- Fractional power: `12.3 ^ 45.6` exact.
- `exp(123.456)` / `exp(1234.5678)` exact (51 / ~540 digits).
- `log(1.234567e-89)` (95 digits), `log(1.23e-89, 6.4689e45)`
  (91 digits), `log(3.1954752e47, 9.4792021e-73)` (80 digits) exact.
- The `8e9000 - div(8e18000 - 1, 9e9000 - 1) * 9` identity = 8.
- Limits still enforced: beyond 131072 integer digits stays 22003;
  `exp(3000)` still 22003.
- Small-value behavior unchanged; big values order/compare/group.
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

s = socket.create_connection(('127.0.0.1', PORT), timeout=60)
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

def err_of(q):
    _, err = do_sql(q)
    return err

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

# A. Exact big literals and big arithmetic (past the old i128 wall).
check("A1 50-digit literal + 1",
      val("select 12345678901234567890123456789012345678901234567890 + 1")
      == "12345678901234567890123456789012345678901234567891")
check("A2 38-digit * 38-digit exact",
      val("select 99999999999999999999999999999999999999 * 99999999999999999999999999999999999999")
      == "9999999999999999999999999999999999999800000000000000000000000000000000000001")
check("A3 big subtraction exact",
      val("select 100000000000000000000000000000000000000 - 1")
      == "99999999999999999999999999999999999999")
check("A4 big division exact",
      val("select div(100000000000000000000000000000000000000, 3)")
      == "33333333333333333333333333333333333333")
check("A5 8e9000 identity",
      val("select 8e9000 - div(8e18000 - 1, 9e9000 - 1) * 9") == "8")

# B. Integer power past the i128 wall (PG19 numeric.out, digit-exact).
check("B1 1.234 ^ 5678 (523 chars)",
      val("select 1.234 ^ 5678")
      == "307239295662090741644584872593956173493568238595074141254349565406661439636598896798876823220904084953233015553994854875890890858118656468658643918169805277399402542281777901029346337707622181574346585989613344285010764501017625366742865066948856161360224801370482171458030533346309750557140549621313515752078638620714732831815297168231790779296290266207315344008883935010274044001522606235576584215999260117523114297033944018699691024106823438431754073086813382242140602291215149759520833200152654884259619588924545324.597")
check("B2 12.3 ^ 45.6",
      val("select 12.3 ^ 45.6")
      == "50081010321492803393171165777624533697036806969694.9")

# C. exp past the i128 wall (PG19 numeric.out, digit-exact).
check("C1 exp(123.456)",
      val("select exp(123.456)")
      == "413294435277809344957685441227343146614594393746575438.725")
check("C2 exp(1234.5678)",
      val("select exp(1234.5678)")
      == "146549072930959479983482138503979804217622199675223653966270157446954995433819741094410764947112047906012815540251009949604426069672532417736057033099274204598385314594846509975629046864798765888104789074984927709616261452461385220475510438783429612447831614003668421849727379202555580791042606170523016207262965336641214601082882495255771621327088265411334088968112458492660609809762865582162764292604697957813514621259353683899630997077707406305730694385703091201347848855199354307506425820147289848677003277208302716466011827836279231.9667")

# D. log with full declared-dscale precision (PG19 numeric.out, digit-exact).
check("D1 log(1.234567e-89) 95 digits",
      val("select log(1.234567e-89)")
      == "-88.90848533591373725637496492944925187293052336306443143312825869985819779294142441287021741054275")
check("D2 log(1.23e-89, 6.4689e45) 91 digits",
      val("select log(1.23e-89, 6.4689e45)")
      == "-0.5152489207781856983977054971756484879653568168479201885425588841094788842469115325262329756")
check("D3 log(3.1954752e47, 9.4792021e-73) 80 digits",
      val("select log(3.1954752e47, 9.4792021e-73)")
      == "-1.51613372350688302142917386143459361608600157692779164475351842333265418126982165")

# E. Limits still enforced; small values unchanged.
check("E1 10^131073 exceeds 131072-digit limit -> 22003",
      err_of("select 10 ^ 131073") == "22003")
# v0.67: PG19's exp_var guard is |x| >= NUMERIC_MAX_RESULT_SCALE * 3 =
# 6000, so exp(3000) is finite (1303 integer digits) and exp(6000)
# overflows with 22003. The old "exp(3000) -> 22003" encoded the
# stale 3000 guard.
check("E2 exp(3000) finite (PG19 guard is 6000)",
      val("select exp(3000::numeric)").startswith(
          "7646200989054704889310727660502434"))
check("E2b exp(6000) -> 22003",
      err_of("select exp(6000::numeric)") == "22003")
check("E3 small add unchanged", val("select 1.5 + 2.25") == "3.75")
check("E4 small div unchanged", val("select 10 / 4") == "2")
check("E5 small power unchanged", val("select 2 ^ 10") == "1024.0000000000000")

# F. Big values order, compare, and group correctly.
check("F1 big comparison",
      val("select 99999999999999999999999999999999999999 < 100000000000000000000000000000000000000")
      == "t")
check("F2 big in GROUP BY",
      val("select count(*) from (values (12345678901234567890123456789012345678901234567890),"
          " (12345678901234567890123456789012345678901234567890), (1)) v(x) group by x order by x limit 1")
      == "1")
check("F3 big ORDER BY",
      val("select x from (values (2), (12345678901234567890123456789012345678901234567890), (1)) v(x) order by x limit 1")
      == "1")

print(f"protocol_test64: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
