#!/usr/bin/env python3
"""v0.19 strings type cluster protocol tests.

Tests for bytea hex/escape/base64, encode/decode, SHA-224/256/384/512,
regexp_* functions, SIMILAR TO, LIKE..ESCAPE, OVERLAY, POSITION,
strpos, translate, unistr, E''/U&'' literals, and split_part fixes.
"""
import socket
import struct
import sys

HOST = "127.0.0.1"
PORT = 5433

def read_msg(sock):
    hdr = b""
    while len(hdr) < 5:
        chunk = sock.recv(5 - len(hdr))
        if not chunk:
            return None, None
        hdr += chunk
    typ = hdr[0:1]
    ln = struct.unpack("!I", hdr[1:5])[0]
    body = b""
    while len(body) < ln - 4:
        chunk = sock.recv(ln - 4 - len(body))
        if not chunk:
            break
        body += chunk
    return typ, body

def startup(sock):
    params = b"user\x00postgres\x00database\x00postgres\x00\x00"
    msg = struct.pack("!IH", len(params) + 8, 3) + struct.pack("!H", 0) + params
    sock.sendall(msg)
    while True:
        typ, body = read_msg(sock)
        if typ is None:
            raise RuntimeError("connection closed")
        if typ == b"Z":
            break

def query(sock, sql):
    q = b"Q" + struct.pack("!I", len(sql) + 5) + sql.encode() + b"\x00"
    sock.sendall(q)
    rows = []
    cols = []
    while True:
        typ, body = read_msg(sock)
        if typ is None:
            raise RuntimeError("connection closed")
        if typ == b"T":
            n = struct.unpack("!H", body[:2])[0]
            off = 2
            cols = []
            for _ in range(n):
                end = body.index(b"\x00", off)
                cols.append(body[off:end].decode())
                off = end + 1
                off += 18  # skip rest of field desc
        elif typ == b"D":
            n = struct.unpack("!H", body[:2])[0]
            off = 2
            row = []
            for _ in range(n):
                ln = struct.unpack("!i", body[off:off+4])[0]
                off += 4
                if ln == -1:
                    row.append(None)
                else:
                    row.append(body[off:off+ln].decode())
                    off += ln
            rows.append(row)
        elif typ == b"C":
            pass
        elif typ == b"Z":
            break
        elif typ == b"E":
            raise RuntimeError(f"query error: {body}")
    return cols, rows

def test(sock, sql, expected):
    cols, rows = query(sock, sql)
    actual = rows[0][0] if rows and rows[0] else None
    if actual != expected:
        print(f"FAIL: {sql}")
        print(f"  expected: {expected!r}")
        print(f"  actual:   {actual!r}")
        return False
    return True

def main():
    sock = socket.create_connection((HOST, PORT), timeout=10)
    try:
        startup(sock)
        passed = 0
        failed = 0
        tests = [
            # bytea hex with whitespace
            ("SELECT '\\xDeAdBeEf'::bytea", "\\xdeadbeef"),
            ("SELECT '\\x De Ad Be Ef '::bytea", "\\xdeadbeef"),
            # bytea functions
            ("SELECT reverse('\\xabcd'::bytea)", "\\xcdab"),
            ("SELECT encode('\\x010203'::bytea, 'hex')", "010203"),
            ("SELECT encode('hello'::bytea, 'base64')", "aGVsbG8="),
            ("SELECT decode('aGVsbG8=', 'base64')", "\\x68656c6c6f"),
            ("SELECT crc32c('hello'::bytea)", "2591144780"),
            # SHA
            ("SELECT sha256('')", "\\xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            ("SELECT sha224('')", "\\xd14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"),
            # regexp
            ("SELECT regexp_replace('1112223333', '(\\d{3})(\\d{3})(\\d{4})', '(\\1) \\2-\\3')", "(111) 222-3333"),
            ("SELECT regexp_count('123123123123123', '(12)3')", "5"),
            ("SELECT regexp_like('Hello', 'hello', 'i')", "t"),
            ("SELECT regexp_substr('abc123def', '\\d+')", "123"),
            ("SELECT regexp_instr('abcdef', 'cd')", "3"),
            # SIMILAR TO
            ("SELECT 'abcdefg' SIMILAR TO 'a%g'", "t"),
            ("SELECT 'abc' SIMILAR TO '_b_'", "t"),
            ("SELECT 'hawkeye' SIMILAR TO 'h%'", "t"),
            # LIKE ESCAPE
            ("SELECT 'hawkeye' LIKE 'h%' ESCAPE '#'", "t"),
            # string functions
            ("SELECT translate('12345', '14', 'ax')", "a23x5"),
            ("SELECT overlay('abcdef' placing '45' from 4)", "abc45f"),
            ("SELECT split_part('joeuser@mydatabase', '@', 2)", "mydatabase"),
            ("SELECT strpos('abcdef', 'cd')", "3"),
            ("SELECT unistr('d\\u0061t')", "dat"),
            # E'' and U&'' literals
            ("SELECT E'a\\nb'", "a\nb"),
            ("SELECT U&'d\\0061t'", "dat"),
            # SUBSTRING regex
            ("SELECT SUBSTRING('abcdefg' FROM 'c.e')", "cde"),
            ("SELECT SUBSTRING('abcdefg' FROM 'b(.*)f')", "cde"),
        ]
        for sql, expected in tests:
            try:
                if test(sock, sql, expected):
                    passed += 1
                else:
                    failed += 1
            except Exception as e:
                print(f"ERROR: {sql}: {e}")
                failed += 1
        print(f"\n{passed} passed, {failed} failed")
        return 0 if failed == 0 else 1
    finally:
        sock.close()

if __name__ == "__main__":
    sys.exit(main())
