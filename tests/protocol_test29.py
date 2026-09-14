#!/usr/bin/env python3
"""rustgres v0.29 protocol tests: bytea I/O fidelity.

RED/GREEN: expectations are taken from PostgreSQL 19's own regression
outputs (tests/conformance/data/expected/strings.out, bytea section):

- SET/SHOW/RESET bytea_output (hex|escape); escape output renders
  non-printables as \\ooo octal and doubles backslashes (PG docs Table 8.8).
- Hex input errors: 22023 "invalid hexadecimal data: odd number of digits",
  22023 'invalid hexadecimal digit: "x"' (char always double-quoted).
- Escape-format input errors stay 22P02 "invalid input syntax for type bytea".
- length(bytea) / octet_length(bytea) return the byte count.
- pg_input_is_valid(text, 'bytea') is a scalar validity probe.

They require a running rustgres server on port 5433.

Run: python3 tests/protocol_test29.py
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
                raise RuntimeError("connection closed")
            d += c
        return d

    while True:
        t = rd(1)
        ln = struct.unpack("!i", rd(4))[0]
        rd(ln - 4)
        if t == b"Z":
            break
    return s, rd


def sql(s, rd, query):
    q = query.encode() + b"\x00"
    s.sendall(b"Q" + struct.pack("!i", len(q) + 4) + q)
    rows = []
    code = None
    msg = ""
    while True:
        t = rd(1)
        ln = struct.unpack("!i", rd(4))[0]
        data = rd(ln - 4)
        if t == b"T":
            pass
        elif t == b"D":
            ncols = struct.unpack("!h", data[:2])[0]
            off = 2
            row = []
            for _ in range(ncols):
                ln2 = struct.unpack("!i", data[off : off + 4])[0]
                off += 4
                if ln2 == -1:
                    row.append(None)
                else:
                    row.append(data[off : off + ln2].decode())
                    off += ln2
            rows.append(row)
        elif t == b"E":
            i = 0
            while i < len(data):
                f = data[i : i + 1]
                i += 1
                try:
                    j = data.index(b"\x00", i)
                except ValueError:
                    break
                v = data[i:j].decode()
                i = j + 1
                if f == b"C":
                    code = v
                if f == b"M":
                    msg = v
                if f == b"\x00":
                    break
        elif t == b"Z":
            break
    return rows, code, msg


def check(name, rows, code, msg, expect_rows=None, expect_code=None, expect_msg=None):
    global CHECKS, PASSED
    CHECKS += 1
    ok = True
    if expect_rows is not None and rows != expect_rows:
        ok = False
    if expect_code is not None and code != expect_code:
        ok = False
    if expect_msg is not None and msg != expect_msg:
        ok = False
    if ok:
        PASSED += 1
    else:
        FAILED.append(
            f"{name}: rows={rows} code={code} msg={msg!r} "
            f"expect_rows={expect_rows} expect_code={expect_code} expect_msg={expect_msg!r}"
        )


def main():
    s, rd = connect()

    # --- bytea_output GUC ---
    r, c, m = sql(s, rd, "SET bytea_output TO hex")
    check("set bytea_output hex", r, c, m, expect_rows=[], expect_code=None)
    r, c, m = sql(s, rd, "SHOW bytea_output")
    check("show bytea_output hex", r, c, m, expect_rows=[["hex"]])
    r, c, m = sql(s, rd, "SET bytea_output TO escape")
    check("set bytea_output escape", r, c, m, expect_rows=[], expect_code=None)
    r, c, m = sql(s, rd, "SHOW bytea_output")
    check("show bytea_output escape", r, c, m, expect_rows=[["escape"]])
    # back to hex for the hex-output tests below
    sql(s, rd, "SET bytea_output TO hex")
    r, c, m = sql(s, rd, "SET bytea_output TO bogus")
    check("set bytea_output bogus", r, c, m, expect_code="22023")
    r, c, m = sql(s, rd, "RESET bytea_output")
    check("reset bytea_output", r, c, m, expect_rows=[], expect_code=None)
    r, c, m = sql(s, rd, "SHOW bytea_output")
    check("show after reset", r, c, m, expect_rows=[["hex"]])

    # --- hex input errors (PG 19 strings.out) ---
    r, c, m = sql(s, rd, r"SELECT E'\\xDeAdBeE'::bytea")
    check("hex odd digits", r, c, m,
          expect_code="22023",
          expect_msg="invalid hexadecimal data: odd number of digits")
    r, c, m = sql(s, rd, r"SELECT E'\\xDeAdBeEx'::bytea")
    check("hex bad digit", r, c, m,
          expect_code="22023",
          expect_msg='invalid hexadecimal digit: "x"')
    r, c, m = sql(s, rd, r"SELECT ('\x' || repeat('!', 32))::bytea")
    check("hex bad digit !", r, c, m,
          expect_code="22023",
          expect_msg='invalid hexadecimal digit: "!"')
    # valid hex still works
    r, c, m = sql(s, rd, r"SELECT E'\\xDeAdBeEf'::bytea")
    check("hex ok", r, c, m, expect_rows=[["\\xdeadbeef"]])
    r, c, m = sql(s, rd, r"SELECT E'\\xDe00BeEf'::bytea")
    check("hex with 00", r, c, m, expect_rows=[["\\xde00beef"]])

    # --- escape-format input errors stay 22P02 ---
    r, c, m = sql(s, rd, r"SELECT E'foo\\99bar'::bytea")
    check("escape bad input", r, c, m,
          expect_code="22P02",
          expect_msg="invalid input syntax for type bytea")

    # --- escape output format (PG docs Table 8.8) ---
    sql(s, rd, "SET bytea_output TO escape")
    # \xDeAdBeEf = [0xDE,0xAD,0xBE,0xEF] -> \336\255\276\357
    r, c, m = sql(s, rd, r"SELECT E'\\xDeAdBeEf'::bytea")
    check("escape output", r, c, m, expect_rows=[["\\336\\255\\276\\357"]])
    # printable ASCII stays literal; backslash doubles
    r, c, m = sql(s, rd, r"SELECT E'DeAdBeEf'::bytea")
    check("escape output printable", r, c, m, expect_rows=[["DeAdBeEf"]])
    r, c, m = sql(s, rd, r"SELECT E'\\134'::bytea")  # single backslash byte
    check("escape output backslash", r, c, m, expect_rows=[["\\\\"]])
    sql(s, rd, "SET bytea_output TO hex")
    r, c, m = sql(s, rd, r"SELECT E'\\xDeAdBeEf'::bytea")
    check("hex output again", r, c, m, expect_rows=[["\\xdeadbeef"]])

    # --- length / octet_length on bytea ---
    r, c, m = sql(s, rd, r"SELECT length('\xdeadbeef'::bytea)")
    check("length bytea", r, c, m, expect_rows=[["4"]])
    r, c, m = sql(s, rd, r"SELECT octet_length('\xdeadbeef'::bytea)")
    check("octet_length bytea", r, c, m, expect_rows=[["4"]])
    r, c, m = sql(s, rd, r"SELECT length(''::bytea)")
    check("length empty bytea", r, c, m, expect_rows=[["0"]])

    # --- pg_input_is_valid (scalar) ---
    r, c, m = sql(s, rd, r"SELECT pg_input_is_valid(E'\\xDeAdBeE', 'bytea')")
    check("pg_input_is_valid false", r, c, m, expect_rows=[["f"]])
    r, c, m = sql(s, rd, r"SELECT pg_input_is_valid(E'\\xDeAdBeEf', 'bytea')")
    check("pg_input_is_valid true", r, c, m, expect_rows=[["t"]])

    s.close()
    print(f"{PASSED}/{CHECKS} passed")
    if FAILED:
        for f in FAILED:
            print("FAIL:", f)
        sys.exit(1)


if __name__ == "__main__":
    main()
