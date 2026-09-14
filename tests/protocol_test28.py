#!/usr/bin/env python3
"""rustgres v0.28 protocol tests: bytea functions, casts, and bytea_output.

RED/GREEN: these tests were written against PostgreSQL 19's
bytea semantics. They require a running rustgres server on
port 5433.

Run: python3 tests/protocol_test28.py
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
                if f == b"\x00":
                    break
        elif t == b"Z":
            break
    return rows, code


def check(name, rows, code, expect_rows=None, expect_code=None):
    global CHECKS, PASSED
    CHECKS += 1
    ok = True
    if expect_rows is not None and rows != expect_rows:
        ok = False
    if expect_code is not None and code != expect_code:
        ok = False
    if ok:
        PASSED += 1
    else:
        FAILED.append(
            f"{name}: rows={rows} code={code} expect_rows={expect_rows} expect_code={expect_code}"
        )


def main():
    s, rd = connect()

    # get_byte / set_byte
    # '\x1234567890abcdef00' = [0x12,0x34,0x56,0x78,0x90,0xab,0xcd,0xef,0x00]
    check("get_byte", *sql(s, rd, r"SELECT get_byte('\x1234567890abcdef00'::bytea, 3)"),
          expect_rows=[["120"]])  # 0x78
    check("set_byte", *sql(s, rd, r"SELECT set_byte('\x1234567890abcdef00'::bytea, 7, 11)"),
          expect_rows=[["\\x1234567890abcd0b00"]])  # byte 7 (0xef) -> 0x0b

    # get_bit / set_bit (bit 0 = MSB of byte 0)
    # byte 5 = 0xab = 10101011; bit 43 -> byte 5, bit_idx 4 -> (0xab>>4)&1 = 0
    check("get_bit", *sql(s, rd, r"SELECT get_bit('\x1234567890abcdef00'::bytea, 43)"),
          expect_rows=[["0"]])
    # set bit 43 to 1: 0xab (10101011) | 0x10 = 0xbb (10111011)
    check("set_bit 1", *sql(s, rd, r"SELECT set_bit('\x1234567890abcdef00'::bytea, 43, 1)"),
          expect_rows=[["\\x1234567890bbcdef00"]])

    # bit_count
    check("bit_count", *sql(s, rd, r"SELECT bit_count('\x1234567890'::bytea)"),
          expect_rows=[["15"]])

    # int2 <-> bytea (big-endian binary)
    check("int2 to bytea", *sql(s, rd, r"SELECT 4660::int2::bytea"),
          expect_rows=[["\\x1234"]])
    check("int2 to bytea neg", *sql(s, rd, r"SELECT (-4660)::int2::bytea"),
          expect_rows=[["\\xedcc"]])
    check("bytea to int2", *sql(s, rd, r"SELECT '\x12'::bytea::int2"),
          expect_rows=[["18"]])
    check("bytea to int2 neg", *sql(s, rd, r"SELECT '\x8000'::bytea::int2"),
          expect_rows=[["-32768"]])
    check("bytea empty to int2", *sql(s, rd, r"SELECT ''::bytea::int2"),
          expect_rows=[["0"]])

    # int4 <-> bytea
    check("int4 to bytea", *sql(s, rd, r"SELECT 305419896::int4::bytea"),
          expect_rows=[["\\x12345678"]])
    check("bytea to int4", *sql(s, rd, r"SELECT '\x12345678'::bytea::int4"),
          expect_rows=[["305419896"]])
    check("bytea to int4 neg", *sql(s, rd, r"SELECT '\x80000000'::bytea::int4"),
          expect_rows=[["-2147483648"]])

    # int8 <-> bytea
    check("int8 to bytea", *sql(s, rd, r"SELECT 1234605616436508552::int8::bytea"),
          expect_rows=[["\\x1122334455667788"]])
    check("bytea to int8", *sql(s, rd, r"SELECT '\x1122334455667788'::bytea::int8"),
          expect_rows=[["1234605616436508552"]])

    s.close()
    print(f"{PASSED}/{CHECKS} passed")
    if FAILED:
        print(f"{len(FAILED)} FAILED:")
        for f in FAILED:
            print(f"  {f}")
        sys.exit(1)


if __name__ == "__main__":
    main()
