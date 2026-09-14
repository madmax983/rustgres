#!/usr/bin/env python3
"""rustgres v0.26 protocol tests: numeric to_char / to_number formatting.

RED/GREEN: these tests were written against PostgreSQL 19's
formatting.c semantics (REL_19_STABLE) and PG's numeric.out
regression expectations. They require a running rustgres server on
port 5433.

Run: python3 tests/protocol_test26.py
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

    # --- v0.26: basic numeric to_char ---
    check("to_char 999.99", *sql(s, rd, "SELECT to_char(100::numeric, '999.99')"),
          expect_rows=[[" 100.00"]])
    check("to_char neg 999.99", *sql(s, rd, "SELECT to_char(-100::numeric, '999.99')"),
          expect_rows=[["-100.00"]])
    check("to_char zero 999.99", *sql(s, rd, "SELECT to_char(0::numeric, '999.99')"),
          expect_rows=[["    .00"]])
    check("to_char round", *sql(s, rd, "SELECT to_char(2.345::numeric, '99.99')"),
          expect_rows=[["  2.35"]])
    check("to_char round up carry", *sql(s, rd, "SELECT to_char(9.99::numeric, '99.9')"),
          expect_rows=[[" 10.0"]])
    check("to_char overflow", *sql(s, rd, "SELECT to_char(12345::numeric, '999')"),
          expect_rows=[[" ###"]])
    check("to_char overflow neg", *sql(s, rd, "SELECT to_char(-12345::numeric, '999')"),
          expect_rows=[["-###"]])
    check("to_char overflow dec", *sql(s, rd, "SELECT to_char(123.456::numeric, '99.99')"),
          expect_rows=[[" ##.##"]])

    # --- v0.26: FM fill mode ---
    check("to_char FM999.9", *sql(s, rd, "SELECT to_char(100::numeric, 'FM999.9')"),
          expect_rows=[["100."]])
    check("to_char FM999.", *sql(s, rd, "SELECT to_char(100::numeric, 'FM999.')"),
          expect_rows=[["100"]])
    check("to_char FM999", *sql(s, rd, "SELECT to_char(100::numeric, 'FM999')"),
          expect_rows=[["100"]])
    check("to_char FM neg", *sql(s, rd, "SELECT to_char(-12.5::numeric, 'FM99.99')"),
          expect_rows=[["-12.5"]])
    check("to_char FM zero", *sql(s, rd, "SELECT to_char(0::numeric, 'FM99.99')"),
          expect_rows=[["0."]])

    # --- v0.26: zero fill ---
    check("to_char 0999", *sql(s, rd, "SELECT to_char(42::numeric, '0999')"),
          expect_rows=[[" 0042"]])
    check("to_char 0.99", *sql(s, rd, "SELECT to_char(0.5::numeric, '0.99')"),
          expect_rows=[[" 0.50"]])

    # --- v0.26: sign styles ---
    check("to_char MI neg", *sql(s, rd, "SELECT to_char(-4.2::numeric, 'MI99.99')"),
          expect_rows=[["- 4.20"]])
    check("to_char MI pos", *sql(s, rd, "SELECT to_char(4.2::numeric, 'MI99.99')"),
          expect_rows=[["  4.20"]])
    check("to_char PL pos", *sql(s, rd, "SELECT to_char(4.2::numeric, 'PL99.99')"),
          expect_rows=[["+ 4.20"]])
    check("to_char PL neg", *sql(s, rd, "SELECT to_char(-4.2::numeric, 'PL99.99')"),
          expect_rows=[["  -4.20"]])
    check("to_char SG neg", *sql(s, rd, "SELECT to_char(-4.2::numeric, 'SG99.99')"),
          expect_rows=[["- 4.20"]])
    check("to_char SG pos", *sql(s, rd, "SELECT to_char(4.2::numeric, 'SG99.99')"),
          expect_rows=[["+ 4.20"]])
    check("to_char S pre", *sql(s, rd, "SELECT to_char(-4.2::numeric, 'S99.99')"),
          expect_rows=[[" -4.20"]])
    check("to_char S pre pos", *sql(s, rd, "SELECT to_char(4.2::numeric, 'S99.99')"),
          expect_rows=[[" +4.20"]])
    check("to_char S post", *sql(s, rd, "SELECT to_char(-4.2::numeric, '99.99S')"),
          expect_rows=[[" 4.20-"]])
    check("to_char PR neg", *sql(s, rd, "SELECT to_char(-123::numeric, '999PR')"),
          expect_rows=[["<123>"]])
    check("to_char PR pos", *sql(s, rd, "SELECT to_char(123::numeric, '999PR')"),
          expect_rows=[[" 123 "]])

    # --- v0.26: grouping ---
    check("to_char G", *sql(s, rd, "SELECT to_char(1234567::numeric, '9G999G999')"),
          expect_rows=[[" 1,234,567"]])
    check("to_char comma", *sql(s, rd, "SELECT to_char(1234567::numeric, '9,999,999')"),
          expect_rows=[[" 1,234,567"]])

    # --- v0.26: roman numerals ---
    check("to_char rn 100", *sql(s, rd, "SELECT to_char(100::numeric, 'rn')"),
          expect_rows=[["              c"]])
    check("to_char rn 1234", *sql(s, rd, "SELECT to_char(1234::numeric, 'rn')"),
          expect_rows=[["       mccxxxiv"]])
    check("to_char fmrn", *sql(s, rd, "SELECT to_char(1237::float8, 'fmrn')"),
          expect_rows=[["mccxxxvii"]])
    check("to_char RN overflow", *sql(s, rd, "SELECT to_char(100e9::numeric, 'RN')"),
          expect_rows=[["###############"]])
    check("to_char RN 4", *sql(s, rd, "SELECT to_char(4::numeric, 'RN')"),
          expect_rows=[["             IV"]])

    # --- v0.26: ordinal suffix ---
    check("to_char th 1", *sql(s, rd, "SELECT to_char(1::numeric, '99th')"),
          expect_rows=[["  1st"]])
    check("to_char th 2", *sql(s, rd, "SELECT to_char(2::numeric, '99TH')"),
          expect_rows=[["  2ND"]])
    check("to_char th 13", *sql(s, rd, "SELECT to_char(13::numeric, '99th')"),
          expect_rows=[[" 13th"]])
    check("to_char th 23", *sql(s, rd, "SELECT to_char(23::numeric, '99th')"),
          expect_rows=[[" 23rd"]])

    # --- v0.26: V (multiply) ---
    check("to_char V", *sql(s, rd, "SELECT to_char(1234.56::numeric, '99999V99')"),
          expect_rows=[["  123456"]])
    check("to_char V float8", *sql(s, rd, "SELECT to_char(1234.56::float8, '99999V99')"),
          expect_rows=[["  123456"]])

    # --- v0.26: EEEE ---
    check("to_char EEEE", *sql(s, rd, "SELECT to_char(12345.678::numeric, '9.999EEEE')"),
          expect_rows=[[" 1.235e+04"]])
    check("to_char EEEE neg", *sql(s, rd, "SELECT to_char(-34338492::numeric, '9.999EEEE')"),
          expect_rows=[["-3.434e+07"]])
    check("to_char EEEE zero", *sql(s, rd, "SELECT to_char(0::numeric, '9.999EEEE')"),
          expect_rows=[[" 0.000e+00"]])

    # --- v0.26: literals and quoting ---
    check("to_char literal", *sql(s, rd, "SELECT to_char(100::numeric, 'foo999')"),
          expect_rows=[["foo 100"]])
    check("to_char quoted", *sql(s, rd, """SELECT to_char(100::numeric, '"foo"999')"""),
          expect_rows=[["foo 100"]])

    # --- v0.26: integer and float input ---
    check("to_char int4", *sql(s, rd, "SELECT to_char(42::int4, '9999')"),
          expect_rows=[["   42"]])
    check("to_char int8", *sql(s, rd, "SELECT to_char(-42::int8, '9999')"),
          expect_rows=[["  -42"]])
    check("to_char float8", *sql(s, rd, "SELECT to_char(4.2::float8, '99.99')"),
          expect_rows=[["  4.20"]])

    # --- v0.26: to_number ---
    check("to_number G", *sql(s, rd, "SELECT to_number('-34,338,492', '99G999G999')"),
          expect_rows=[["-34338492"]])
    check("to_number G D", *sql(s, rd, "SELECT to_number('-34,338,492.654,878', '99G999G999D999G999')"),
          expect_rows=[["-34338492.654878"]])
    check("to_number PR", *sql(s, rd, "SELECT to_number('<564646.654564>', '999999.999999PR')"),
          expect_rows=[["-564646.654564"]])
    check("to_number S post", *sql(s, rd, "SELECT to_number('0.00001-', '9.999999S')"),
          expect_rows=[["-0.00001"]])
    check("to_number MI", *sql(s, rd, "SELECT to_number('5.01-', 'FM9.999999MI')"),
          expect_rows=[["-5.01"]])
    check("to_number spaces", *sql(s, rd, "SELECT to_number('5 4 4 4 4 8. 7 8', '9 9 9 9 9 9. 9 9')"),
          expect_rows=[["544448.78"]])
    check("to_number FM dot", *sql(s, rd, "SELECT to_number('.01', 'FM9.99')"),
          expect_rows=[["0.01"]])
    check("to_number S pre", *sql(s, rd, "SELECT to_number('.-01', 'S99.99')"),
          expect_rows=[["-0.01"]])
    check("to_number comma", *sql(s, rd, "SELECT to_number('34,50', '999,99')"),
          expect_rows=[["3450"]])
    check("to_number 999G", *sql(s, rd, "SELECT to_number('123,000', '999G')"),
          expect_rows=[["123"]])
    check("to_number L", *sql(s, rd, "SELECT to_number('$1,234.56', 'L99,999.99')"),
          expect_rows=[["1234.56"]])
    check("to_number th", *sql(s, rd, "SELECT to_number('42nd', '99th')"),
          expect_rows=[["42"]])
    check("to_number V", *sql(s, rd, "SELECT to_number('123456', '99999V99')"),
          expect_rows=[["1234.560000000000000000"]])

    # --- v0.26: roman input ---
    check("to_number rn", *sql(s, rd, "SELECT to_number('XIV', 'RN')"),
          expect_rows=[["14"]])
    check("to_number rn lower", *sql(s, rd, "SELECT to_number('xiv', 'rn')"),
          expect_rows=[["14"]])
    check("to_number rn bad", *sql(s, rd, "SELECT to_number('VV', 'RN')"),
          expect_code="22P02")
    check("to_number rn bad sub", *sql(s, rd, "SELECT to_number('IL', 'RN')"),
          expect_code="22P02")
    check("to_number rn bad repeat", *sql(s, rd, "SELECT to_number('MMMM', 'RN')"),
          expect_code="22P02")

    # --- v0.26: invalid pictures -> 42601 ---
    check("to_char 9 after PR", *sql(s, rd, "SELECT to_char(1::numeric, 'PR999')"),
          expect_code="42601")
    check("to_char RN twice", *sql(s, rd, "SELECT to_char(1::numeric, 'RNRN')"),
          expect_code="42601")
    check("to_char EEEE twice", *sql(s, rd, "SELECT to_char(1::numeric, 'EEEEEEEE')"),
          expect_code="42601")
    check("to_char EEEE with FM", *sql(s, rd, "SELECT to_char(1::numeric, 'FMEEEE')"),
          expect_code="42601")
    check("to_char V with dot", *sql(s, rd, "SELECT to_char(1::numeric, '99V99.9')"),
          expect_code="42601")
    check("to_char RN with S", *sql(s, rd, "SELECT to_char(1::numeric, 'SNRN')"),
          expect_code="42601")
    check("to_number EEEE", *sql(s, rd, "SELECT to_number('1', 'EEEE')"),
          expect_code="0A000")

    # --- v0.26: NULLs ---
    check("to_char null val", *sql(s, rd, "SELECT to_char(NULL::numeric, '999')"),
          expect_rows=[[None]])
    check("to_char null fmt", *sql(s, rd, "SELECT to_char(1::numeric, NULL)"),
          expect_rows=[[None]])
    check("to_number null", *sql(s, rd, "SELECT to_number(NULL, '999')"),
          expect_rows=[[None]])

    print(f"{PASSED}/{CHECKS} passed")
    if FAILED:
        print(f"{len(FAILED)} FAILED:")
        for name, rows, code, erows, ecode in FAILED:
            print(f"  {name}: rows={rows} code={code} expect_rows={erows} expect_code={ecode}")
        sys.exit(1)


if __name__ == "__main__":
    main()
