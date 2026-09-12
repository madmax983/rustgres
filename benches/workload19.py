#!/usr/bin/env python3
"""v0.19 string/bytea-heavy workload benchmark.

Measures throughput of the string/bytea paths added in v0.19:
bytea hex I/O, encode/decode, regexp_*, SIMILAR TO, LIKE..ESCAPE, and
string functions (translate, overlay, split_part, etc.).
"""
import socket
import struct
import sys
import time

HOST = "127.0.0.1"
PORT = 5433

QUERIES = [
    # bytea hex I/O
    "SELECT '\\xDeAdBeEf'::bytea",
    "SELECT '\\x De Ad Be Ef '::bytea",
    "SELECT reverse('\\xabcd'::bytea)",
    "SELECT encode('\\x010203'::bytea, 'base64')",
    "SELECT decode('aGVsbG8=', 'base64')",
    "SELECT crc32c('hello'::bytea)",
    # sha
    "SELECT sha256('The quick brown fox jumps over the lazy dog.')",
    "SELECT sha512('abc')",
    # regexp
    "SELECT regexp_replace('1112223333', '(\\d{3})(\\d{3})(\\d{4})', '(\\1) \\2-\\3')",
    "SELECT regexp_count('123123123123123', '(12)3')",
    "SELECT regexp_like('Hello World', 'hello', 'i')",
    "SELECT regexp_substr('abc123def', '\\d+')",
    # SIMILAR TO / LIKE
    "SELECT 'abcdefg' SIMILAR TO 'a%g'",
    "SELECT 'hawkeye' LIKE 'h%' ESCAPE '#'",
    "SELECT 'abc' SIMILAR TO '_b_'",
    # string functions
    "SELECT translate('12345', '14', 'ax')",
    "SELECT overlay('abcdef' placing '45' from 4)",
    "SELECT split_part('joeuser@mydatabase', '@', 2)",
    "SELECT strpos('abcdef', 'cd')",
    "SELECT unistr('d\\u0061t')",
    # E'' and U&'' literals
    "SELECT E'a\\nb'",
    "SELECT U&'d\\0061t'",
]

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
    msg = struct.pack("!I", 8 + len(params)) + struct.pack("!I", 196608) + params
    # Actually protocol version 3.0 = 196608
    msg = struct.pack("!IH", len(params) + 8, 3) + struct.pack("!H", 0) + params
    sock.sendall(msg)
    # Read until ReadyForQuery
    while True:
        typ, body = read_msg(sock)
        if typ is None:
            raise RuntimeError("connection closed")
        if typ == b"R":  # Auth
            code = struct.unpack("!I", body[:4])[0]
            if code == 0:
                continue
            raise RuntimeError(f"auth failed: {code}")
        if typ == b"Z":
            break

def simple_query(sock, sql):
    q = b"Q" + struct.pack("!I", len(sql) + 5) + sql.encode() + b"\x00"
    sock.sendall(q)
    rows = 0
    while True:
        typ, body = read_msg(sock)
        if typ is None:
            raise RuntimeError("connection closed")
        if typ == b"D":
            rows += 1
        elif typ == b"C":
            pass
        elif typ == b"Z":
            break
        elif typ == b"E":
            raise RuntimeError(f"query error: {body}")
    return rows

def main():
    sock = socket.create_connection((HOST, PORT), timeout=10)
    try:
        startup(sock)
        # Warmup
        for _ in range(5):
            for q in QUERIES:
                simple_query(sock, q)
        # Timed run
        n_iter = 20
        start = time.perf_counter()
        total = 0
        for _ in range(n_iter):
            for q in QUERIES:
                simple_query(sock, q)
                total += 1
        elapsed = time.perf_counter() - start
        qps = total / elapsed if elapsed > 0 else 0
        print(f"{total} queries in {elapsed:.2f}s, {qps:.1f} qps")
    finally:
        sock.close()

if __name__ == "__main__":
    main()
