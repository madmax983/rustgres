#!/usr/bin/env python3
"""rustgres v0.27 protocol tests: regexp function family.

RED/GREEN: these tests were written against PostgreSQL 19's
regexp semantics (REL_19_STABLE) and PG's strings.out
regression expectations. They require a running rustgres server on
port 5433.

Run: python3 tests/protocol_test27.py
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
            # Parse DataRow: int16 ncols, then per-col int32 len + bytes
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
            # ErrorResponse: find 'C' field (SQLSTATE)
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
            f"  {name}: rows={rows} code={code} expect_rows={expect_rows} expect_code={expect_code}"
        )


def main():
    s, rd = connect()

    # regexp_instr: 4th occurrence doesn't exist -> 0
    check(
        "regexp_instr occurrence not found",
        *sql(s, rd, "SELECT regexp_instr('abcabcabc', 'a.c', 1, 4)"),
        expect_rows=[["0"]],
    )

    # regexp_instr: 2nd occurrence exists
    check(
        "regexp_instr occurrence found",
        *sql(s, rd, "SELECT regexp_instr('abcabcabc', 'a.c', 1, 2)"),
        expect_rows=[["4"]],
    )

    # regexp_substr: 4th occurrence doesn't exist -> NULL
    check(
        "regexp_substr occurrence not found",
        *sql(s, rd, "SELECT regexp_substr('abcabcabc', 'a.c', 1, 4) IS NULL"),
        expect_rows=[["t"]],
    )

    # regexp_split_to_array: basic split
    check(
        "regexp_split_to_array basic",
        *sql(s, rd, "SELECT regexp_split_to_array('123456','1')"),
        expect_rows=[['{"",23456}']],
    )

    # regexp_split_to_array: no match
    check(
        "regexp_split_to_array no match",
        *sql(s, rd, "SELECT regexp_split_to_array('123456','nomatch')"),
        expect_rows=[["{123456}"]],
    )

    # regexp_split_to_array: empty pattern
    check(
        "regexp_split_to_array empty pattern",
        *sql(s, rd, "SELECT regexp_split_to_array('123456','')"),
        expect_rows=[["{1,2,3,4,5,6}"]],
    )

    s.close()

    print(f"{PASSED}/{CHECKS} passed")
    if FAILED:
        print(f"{len(FAILED)} FAILED:")
        for f in FAILED:
            print(f)
        sys.exit(1)


if __name__ == "__main__":
    main()
