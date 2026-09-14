#!/usr/bin/env python3
"""rustgres v0.28 protocol tests: bytea functions, casts, and 0x literals.

RED/GREEN: expectations are taken from PostgreSQL's own semantics:
- PG docs: get_bit/set_bit "number bits from the right within each byte;
  bit 0 is the least significant bit of the first byte, and bit 15 is the
  most significant bit of the second byte."
- PG 18+ commit 6da469ba ("Allow casting between bytea and integer types",
  in PG 19): int<->bytea casts are two's complement, most significant byte
  first; bytea->int reinterprets at the target width and raises 22003
  "<type> out of range" only when the bytea is longer than the width.
- PG 19 strings.sql/strings.out regression expectations for the exact
  get_bit/get_byte/set_byte/set_bit examples and error messages.

They require a running rustgres server on port 5433.

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

    # get_byte / set_byte (PG 19 strings.out expectations)
    # '\x1234567890abcdef00' = [0x12,0x34,0x56,0x78,0x90,0xab,0xcd,0xef,0x00]
    check("get_byte", *sql(s, rd, r"SELECT get_byte('\x1234567890abcdef00'::bytea, 3)"),
          expect_rows=[["120"]])  # 0x78
    check("set_byte", *sql(s, rd, r"SELECT set_byte('\x1234567890abcdef00'::bytea, 7, 11)"),
          expect_rows=[["\\x1234567890abcd0b00"]])  # byte 7 (0xef) -> 0x0b
    check("get_byte oob", *sql(s, rd, r"SELECT get_byte('\x1234567890abcdef00'::bytea, 99)"),
          expect_code="22000")
    check("set_byte oob", *sql(s, rd, r"SELECT set_byte('\x1234567890abcdef00'::bytea, 99, 11)"),
          expect_code="22000")

    # get_bit / set_bit: PG numbers bits from the right within each byte
    # (bit 0 = LSB of byte 0). PG 19 strings.out:
    #   get_bit('\x1234567890abcdef00'::bytea, 43) -> 1
    #   set_bit('\x1234567890abcdef00'::bytea, 43, 0) -> \x1234567890a3cdef00
    # (byte 5 = 0xab = 10101011; bit 43 is bit 3 of byte 5, LSB-first)
    check("get_bit", *sql(s, rd, r"SELECT get_bit('\x1234567890abcdef00'::bytea, 43)"),
          expect_rows=[["1"]])
    check("set_bit 0", *sql(s, rd, r"SELECT set_bit('\x1234567890abcdef00'::bytea, 43, 0)"),
          expect_rows=[["\\x1234567890a3cdef00"]])
    check("get_bit oob", *sql(s, rd, r"SELECT get_bit('\x1234567890abcdef00'::bytea, 99)"),
          expect_code="22000")
    check("set_bit oob", *sql(s, rd, r"SELECT set_bit('\x1234567890abcdef00'::bytea, 99, 0)"),
          expect_code="22000")
    check("set_bit bad value", *sql(s, rd, r"SELECT set_bit('\x00'::bytea, 0, 2)"),
          expect_code="22000")
    # Asymmetric bytes pin the LSB-first numbering down:
    # bit 0 of 0x01 is 1; bit 0 of 0x80 is 0; bit 7 of 0x80 is 1.
    check("get_bit lsb", *sql(s, rd, r"SELECT get_bit('\x01'::bytea, 0)"),
          expect_rows=[["1"]])
    check("get_bit msb", *sql(s, rd, r"SELECT get_bit('\x80'::bytea, 0)"),
          expect_rows=[["0"]])
    check("get_bit msb7", *sql(s, rd, r"SELECT get_bit('\x80'::bytea, 7)"),
          expect_rows=[["1"]])
    # set_bit('\x00', 0, 1) sets the least significant bit -> \x01
    check("set_bit lsb", *sql(s, rd, r"SELECT set_bit('\x00'::bytea, 0, 1)"),
          expect_rows=[["\\x01"]])

    # bit_count
    check("bit_count", *sql(s, rd, r"SELECT bit_count('\x1234567890'::bytea)"),
          expect_rows=[["15"]])

    # PG 16+ non-decimal integer literals
    check("hex literal", *sql(s, rd, r"SELECT 0x1234"), expect_rows=[["4660"]])
    check("hex literal neg", *sql(s, rd, r"SELECT -0x1234"), expect_rows=[["-4660"]])
    check("oct literal", *sql(s, rd, r"SELECT 0o17"), expect_rows=[["15"]])
    check("bin literal", *sql(s, rd, r"SELECT 0b101"), expect_rows=[["5"]])
    check("hex literal big", *sql(s, rd, r"SELECT 0x1122334455667788"),
          expect_rows=[["1234605616436508552"]])
    check("bare 0x", *sql(s, rd, r"SELECT 0x"), expect_code="42601")
    # PG16+: underscores only between digits; identifier chars glued to a
    # non-decimal literal are "trailing junk" (42601), never an alias.
    check("hex underscore", *sql(s, rd, r"SELECT 0x1_FF"), expect_rows=[["511"]])
    check("hex doubled underscore", *sql(s, rd, r"SELECT 0x1__2"), expect_code="42601")
    check("hex trailing underscore", *sql(s, rd, r"SELECT 0x1_"), expect_code="42601")
    check("hex leading underscore", *sql(s, rd, r"SELECT 0x_12"), expect_code="42601")
    check("hex trailing junk", *sql(s, rd, r"SELECT 0x1G"), expect_code="42601")
    check("bin bad digit", *sql(s, rd, r"SELECT 0b12"), expect_code="42601")
    check("oct bad digit", *sql(s, rd, r"SELECT 0o89"), expect_code="42601")

    # int2 <-> bytea (PG 19 strings.out expectations, two's complement,
    # most significant byte first)
    check("int2 to bytea", *sql(s, rd, r"SELECT 0x1234::int2::bytea"),
          expect_rows=[["\\x1234"]])
    check("int2 to bytea neg", *sql(s, rd, r"SELECT (-0x1234)::int2::bytea"),
          expect_rows=[["\\xedcc"]])
    check("bytea to int2", *sql(s, rd, r"SELECT '\x12'::bytea::int2"),
          expect_rows=[["18"]])
    check("bytea to int2 neg", *sql(s, rd, r"SELECT '\x8000'::bytea::int2"),
          expect_rows=[["-32768"]])
    check("bytea empty to int2", *sql(s, rd, r"SELECT ''::bytea::int2"),
          expect_rows=[["0"]])
    # Short bytea reinterprets at the target width: '\xFF' -> 255, not -1
    check("bytea to int2 ff", *sql(s, rd, r"SELECT '\xFF'::bytea::int2"),
          expect_rows=[["255"]])
    check("bytea too long int2", *sql(s, rd, r"SELECT '\x123456'::bytea::int2"),
          expect_code="22003")

    # int4 <-> bytea
    check("int4 to bytea", *sql(s, rd, r"SELECT 0x12345678::int4::bytea"),
          expect_rows=[["\\x12345678"]])
    check("int4 to bytea neg", *sql(s, rd, r"SELECT (-0x12345678)::int4::bytea"),
          expect_rows=[["\\xedcba988"]])
    check("bytea to int4", *sql(s, rd, r"SELECT '\x12345678'::bytea::int4"),
          expect_rows=[["305419896"]])
    check("bytea to int4 neg", *sql(s, rd, r"SELECT '\x80000000'::bytea::int4"),
          expect_rows=[["-2147483648"]])
    # '\x8000' as int4 is positive: sign comes from the target width
    check("bytea to int4 8000", *sql(s, rd, r"SELECT '\x8000'::bytea::int4"),
          expect_rows=[["32768"]])
    check("bytea too long int4", *sql(s, rd, r"SELECT '\x123456789A'::bytea::int4"),
          expect_code="22003")

    # int8 <-> bytea
    check("int8 to bytea", *sql(s, rd, r"SELECT 0x1122334455667788::int8::bytea"),
          expect_rows=[["\\x1122334455667788"]])
    check("int8 to bytea neg", *sql(s, rd, r"SELECT (-0x1122334455667788)::int8::bytea"),
          expect_rows=[["\\xeeddccbbaa998878"]])
    check("bytea to int8", *sql(s, rd, r"SELECT '\x1122334455667788'::bytea::int8"),
          expect_rows=[["1234605616436508552"]])
    check("bytea to int8 min", *sql(s, rd, r"SELECT '\x8000000000000000'::bytea::int8"),
          expect_rows=[["-9223372036854775808"]])
    check("bytea to int8 max", *sql(s, rd, r"SELECT '\x7FFFFFFFFFFFFFFF'::bytea::int8"),
          expect_rows=[["9223372036854775807"]])
    check("bytea too long int8", *sql(s, rd, r"SELECT '\x112233445566778899'::bytea::int8"),
          expect_code="22003")

    s.close()
    print(f"{PASSED}/{CHECKS} passed")
    if FAILED:
        print(f"{len(FAILED)} FAILED:")
        for f in FAILED:
            print(f"  {f}")
        sys.exit(1)


if __name__ == "__main__":
    main()
