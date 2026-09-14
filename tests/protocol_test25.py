#!/usr/bin/env python3
"""rustgres v0.25 protocol tests: integer input formats, bitwise operators,
numeric edge cases.

RED/GREEN: these tests were written before the v0.25 implementation and
failed on v0.24. They require a running rustgres server (started by the
test runner or manually on port 5433).

Run: python3 tests/protocol_test25.py
"""

import socket
import struct
import sys

PORT = 5433
HOST = "127.0.0.1"

CHECKS = 0
PASSED = 0
FAILED = []


def connect():
    s = socket.create_connection((HOST, PORT), timeout=10)
    params = b"user\x00postgres\x00\x00"
    body = struct.pack("!i", 196608) + params
    s.sendall(struct.pack("!i", len(body) + 4) + body)

    def rd(n):
        d = b""
        while len(d) < n:
            c = s.recv(n - len(d))
            if not c:
                raise ConnectionError("closed")
            d += c
        return d

    while True:
        t = rd(1)
        ln = struct.unpack("!i", rd(4))[0]
        rd(ln - 4)
        if t == b"Z":
            break
    return s, rd


def sql(s, rd, q):
    body = q.encode() + b"\x00"
    s.sendall(b"Q" + struct.pack("!i", len(body) + 4) + body)
    rows, err = [], None
    while True:
        t = rd(1)
        ln = struct.unpack("!i", rd(4))[0]
        p = rd(ln - 4)
        if t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            pos = 2
            row = []
            for _ in range(n):
                ln2 = struct.unpack("!i", p[pos : pos + 4])[0]
                pos += 4
                if ln2 < 0:
                    row.append(None)
                else:
                    row.append(p[pos : pos + ln2].decode())
                    pos += ln2
            rows.append(row)
        elif t == b"E":
            err = p
        elif t == b"Z":
            break
    return rows, err


def err_code(e):
    if not e:
        return None
    try:
        j = e.index(b"\x00C") + 2
        k = e.index(b"\x00", j)
        return e[j:k].decode()
    except ValueError:
        return "?"


def check(name, rows, err, expect_rows=None, expect_code=None):
    global CHECKS, PASSED
    CHECKS += 1
    code = err_code(err)
    if expect_code:
        ok = code == expect_code
    else:
        ok = not err and rows == expect_rows
    if ok:
        PASSED += 1
    else:
        FAILED.append((name, rows, code, expect_rows, expect_code))


def main():
    s, rd = connect()

    # --- v0.25: non-decimal integer input (PG 16+) ---
    check("int8 0b", *sql(s, rd, "SELECT int8 '0b100101'"), expect_rows=[["37"]])
    check("int8 0o", *sql(s, rd, "SELECT int8 '0o273'"), expect_rows=[["187"]])
    check("int8 0x", *sql(s, rd, "SELECT int8 '0x42F'"), expect_rows=[["1071"]])
    check("int8 0b max", *sql(s, rd, "SELECT int8 '0b111111111111111111111111111111111111111111111111111111111111111'"), expect_rows=[["9223372036854775807"]])
    check("int8 0x max", *sql(s, rd, "SELECT int8 '0x7FFFFFFFFFFFFFFF'"), expect_rows=[["9223372036854775807"]])
    check("int8 -0x min", *sql(s, rd, "SELECT int8 '-0x8000000000000000'"), expect_rows=[["-9223372036854775808"]])
    check("int8 0x overflow", *sql(s, rd, "SELECT int8 '0x8000000000000000'"), expect_code="22003")
    check("int8 -0x overflow", *sql(s, rd, "SELECT int8 '-0x8000000000000001'"), expect_code="22003")
    check("int8 bare 0b", *sql(s, rd, "SELECT int8 '0b'"), expect_code="22P02")
    check("int8 bare 0x", *sql(s, rd, "SELECT int8 '0x'"), expect_code="22P02")
    check("int8 0b bad digit", *sql(s, rd, "SELECT int8 '0b102'"), expect_code="22P02")
    check("int8 0o bad digit", *sql(s, rd, "SELECT int8 '0o888'"), expect_code="22P02")

    # --- v0.25: underscore separators (PG 16+) ---
    check("int8 underscore", *sql(s, rd, "SELECT int8 '1_000_000'"), expect_rows=[["1000000"]])
    check("int8 underscore multi", *sql(s, rd, "SELECT int8 '1_2_3'"), expect_rows=[["123"]])
    check("int8 0x underscore", *sql(s, rd, "SELECT int8 '0x1EEE_FFFF'"), expect_rows=[["518979583"]])
    check("int8 0o underscore", *sql(s, rd, "SELECT int8 '0o2_73'"), expect_rows=[["187"]])
    check("int8 0b underscore", *sql(s, rd, "SELECT int8 '0b_10_0101'"), expect_rows=[["37"]])
    check("int8 underscore leading", *sql(s, rd, "SELECT int8 '_100'"), expect_code="22P02")
    check("int8 underscore trailing", *sql(s, rd, "SELECT int8 '100_'"), expect_code="22P02")
    check("int8 underscore doubled", *sql(s, rd, "SELECT int8 '1__2'"), expect_code="22P02")
    check("int4 0x", *sql(s, rd, "SELECT int4 '0x7FFFFFFF'"), expect_rows=[["2147483647"]])
    check("int4 0x overflow", *sql(s, rd, "SELECT int4 '0x80000000'"), expect_code="22003")
    check("int2 0b", *sql(s, rd, "SELECT int2 '0b_10_0101'"), expect_rows=[["37"]])

    # --- v0.25: numeric input with underscores and base prefixes ---
    check("numeric underscore", *sql(s, rd, "SELECT '12_000_000_000'::numeric"), expect_rows=[["12000000000"]])
    check("numeric underscore frac", *sql(s, rd, "SELECT '12_000.123_456'::numeric"), expect_rows=[["12000.123456"]])
    check("numeric underscore exp", *sql(s, rd, "SELECT '23_000_000_000e-1_0'::numeric"), expect_rows=[["2.3"]])
    check("numeric 0b", *sql(s, rd, "SELECT '0b101'::numeric"), expect_rows=[["5"]])
    check("numeric 0x", *sql(s, rd, "SELECT '0xFF'::numeric"), expect_rows=[["255"]])
    check("numeric +NaN rejected", *sql(s, rd, "SELECT '+NaN'::numeric"), expect_code="22P02")
    check("numeric -NaN rejected", *sql(s, rd, "SELECT '-NaN'::numeric"), expect_code="22P02")
    check("numeric NaN ok", *sql(s, rd, "SELECT 'NaN'::numeric"), expect_rows=[["NaN"]])

    # --- v0.25: bitwise operators ---
    check("bitand", *sql(s, rd, "SELECT 123 & 456"), expect_rows=[["72"]])
    check("bitor", *sql(s, rd, "SELECT 123 | 456"), expect_rows=[["507"]])
    check("bitxor", *sql(s, rd, "SELECT 123 # 456"), expect_rows=[["435"]])
    check("bitnot", *sql(s, rd, "SELECT ~123"), expect_rows=[["-124"]])
    check("shl", *sql(s, rd, "SELECT 5 << 2"), expect_rows=[["20"]])
    check("shr", *sql(s, rd, "SELECT 20 >> 2"), expect_rows=[["5"]])
    check("bitand int2", *sql(s, rd, "SELECT 1::int2 & 2::int2"), expect_rows=[["0"]])
    check("bitnot int2", *sql(s, rd, "SELECT ~1::int2"), expect_rows=[["-2"]])
    check("bitnot int8", *sql(s, rd, "SELECT ~1::int8"), expect_rows=[["-2"]])
    check("shl int8", *sql(s, rd, "SELECT (-1::int8 << 63)::text"), expect_rows=[["-9223372036854775808"]])
    check("precedence", *sql(s, rd, "SELECT 7 # 3 | 8"), expect_rows=[["12"]])
    check("bitand null", *sql(s, rd, "SELECT NULL::int4 & 1"), expect_rows=[[None]])
    check("bitand float err", *sql(s, rd, "SELECT 1.5 & 2"), expect_code="42883")
    check("bitand text err", *sql(s, rd, "SELECT 'a' & 'b'"), expect_code="42883")

    # --- v0.25: float->numeric non-finite ---
    check("float8 NaN to numeric", *sql(s, rd, "SELECT 'NaN'::float8::numeric"), expect_rows=[["NaN"]])
    check("float8 Inf to numeric", *sql(s, rd, "SELECT 'Infinity'::float8::numeric"), expect_rows=[["Infinity"]])
    check("float8 -Inf to numeric", *sql(s, rd, "SELECT '-Infinity'::float8::numeric"), expect_rows=[["-Infinity"]])
    check("float4 NaN to numeric", *sql(s, rd, "SELECT 'NaN'::float4::numeric"), expect_rows=[["NaN"]])

    # --- v0.25: numeric edge cases ---
    check("div nan 0", *sql(s, rd, "SELECT div('nan'::numeric, '0')"), expect_rows=[["NaN"]])
    check("mod nan 0", *sql(s, rd, "SELECT 'nan'::numeric % '0'"), expect_rows=[["NaN"]])
    check("width_bucket nan", *sql(s, rd, "SELECT width_bucket('NaN', 3.0, 4.0, 888)"), expect_rows=[["889"]])
    check("sqrt inf", *sql(s, rd, "SELECT sqrt('inf'::numeric)"), expect_rows=[["Infinity"]])
    check("sqrt nan", *sql(s, rd, "SELECT sqrt('nan'::numeric)"), expect_rows=[["NaN"]])
    check("power text args", *sql(s, rd, "SELECT power('-1'::numeric, 'inf')"), expect_rows=[["1"]])
    check("power neg base", *sql(s, rd, "SELECT power('-2'::numeric, '3')"), expect_rows=[["-8"]])
    check("lcm overflow", *sql(s, rd, "SELECT lcm(9999 * (10::numeric)^131068 + (10::numeric^131068 - 1), 2)"), expect_code="22003")
    check("gcd int4 overflow", *sql(s, rd, "SELECT gcd((-2147483648)::int4, 0::int4)"), expect_code="22003")
    check("lcm int4 overflow", *sql(s, rd, "SELECT lcm(2147483647::int4, 2147483646::int4)"), expect_code="22003")

    # --- v0.25: int2 arithmetic promotes to integer; float rounding ---
    # PG: (-32768)::int2 * (-1)::int2 = 32768 :: integer (no overflow).
    check("int2 mul promotes", *sql(s, rd, "SELECT (-32768)::int2 * (-1)::int2"), expect_rows=[["32768"]])
    check("int2 div promotes", *sql(s, rd, "SELECT (-32768)::int2 / (-1)::int2"), expect_rows=[["32768"]])
    check("float to int2 bankers", *sql(s, rd, "SELECT (2.5::float8)::int2"), expect_rows=[["2"]])
    check("float to int2 bankers neg", *sql(s, rd, "SELECT (-2.5::float8)::int2"), expect_rows=[["-2"]])
    check("float to int4 bankers", *sql(s, rd, "SELECT (1.5::float8)::int4"), expect_rows=[["2"]])

    # --- v0.25: additional input format edge cases ---
    check("int8 0B uppercase", *sql(s, rd, "SELECT int8 '0B101'"), expect_rows=[["5"]])
    check("int8 0O uppercase", *sql(s, rd, "SELECT int8 '0O17'"), expect_rows=[["15"]])
    check("int8 0X uppercase", *sql(s, rd, "SELECT int8 '0Xff'"), expect_rows=[["255"]])
    check("int8 hex mixed case", *sql(s, rd, "SELECT int8 '0xAbC'"), expect_rows=[["2748"]])
    check("int8 neg hex", *sql(s, rd, "SELECT int8 '-0x10'"), expect_rows=[["-16"]])
    check("int8 pos hex", *sql(s, rd, "SELECT int8 '+0x10'"), expect_rows=[["16"]])
    check("int8 0o max", *sql(s, rd, "SELECT int8 '0o777777777777777777777'"), expect_rows=[["9223372036854775807"]])
    check("int8 0o overflow", *sql(s, rd, "SELECT int8 '0o1000000000000000000000'"), expect_code="22003")
    check("int8 underscore after 0x", *sql(s, rd, "SELECT int8 '0x_FF'"), expect_rows=[["255"]])
    check("int8 0x empty after underscore", *sql(s, rd, "SELECT int8 '0x_'"), expect_code="22P02")
    check("int2 0x", *sql(s, rd, "SELECT int2 '0x7FFF'"), expect_rows=[["32767"]])
    check("int2 0x overflow", *sql(s, rd, "SELECT int2 '0x8000'"), expect_code="22003")
    check("numeric 0o", *sql(s, rd, "SELECT '0o17'::numeric"), expect_rows=[["15"]])
    check("numeric neg 0b", *sql(s, rd, "SELECT '-0b101'::numeric"), expect_rows=[["-5"]])
    check("numeric underscore only", *sql(s, rd, "SELECT '_'::numeric"), expect_code="22P02")

    # --- v0.25: additional bitwise edge cases ---
    check("bitand bigint", *sql(s, rd, "SELECT 9223372036854775807::int8 & 1"), expect_rows=[["1"]])
    check("bitor bigint", *sql(s, rd, "SELECT 0::int8 | -1::int8"), expect_rows=[["-1"]])
    check("bitxor zero", *sql(s, rd, "SELECT 5 # 5"), expect_rows=[["0"]])
    check("shl zero", *sql(s, rd, "SELECT 1 << 0"), expect_rows=[["1"]])
    check("shr negative", *sql(s, rd, "SELECT -8 >> 2"), expect_rows=[["-2"]])
    check("bitnot zero", *sql(s, rd, "SELECT ~0"), expect_rows=[["-1"]])
    check("bitwise chain", *sql(s, rd, "SELECT 15 & 7 | 8 # 3"), expect_rows=[["15"]])
    check("shift precedence", *sql(s, rd, "SELECT 1 << 2 + 1"), expect_rows=[["8"]])
    check("bitand int4 int8", *sql(s, rd, "SELECT 1::int4 & 1::int8"), expect_rows=[["1"]])
    check("int8 neg 0b min", *sql(s, rd, "SELECT int8 '-0b1000000000000000000000000000000000000000000000000000000000000000'"), expect_rows=[["-9223372036854775808"]])
    check("numeric 0x underscore", *sql(s, rd, "SELECT '0x1E_EE'::numeric"), expect_rows=[["7918"]])

    s.close()

    print(f"v0.25 protocol: {PASSED}/{CHECKS} passed")
    if FAILED:
        print(f"\n{len(FAILED)} FAILED:")
        for name, rows, code, erows, ecode in FAILED[:10]:
            print(f"  {name}: rows={rows} code={code} expected rows={erows} code={ecode}")
        sys.exit(1)


if __name__ == "__main__":
    main()
