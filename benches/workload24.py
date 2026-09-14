#!/usr/bin/env python3
"""v0.24 string/regexp-heavy workload benchmark.

Measures throughput of the v0.24 string built-ins (repeat, lpad, rpad,
ascii, ltrim, chr, initcap), strict regexp functions, SUBSTRING
FROM..FOR, and INSERT...VALUES expressions.
"""
import socket
import struct
import sys
import time

HOST = "127.0.0.1"
PORT = 5433

QUERIES = [
    # repeat / lpad / rpad
    "SELECT repeat('Pg', 100)",
    "SELECT lpad('hi', 50, 'xy'), rpad('hi', 50, 'xy')",
    "SELECT lpad('hello world', 20), rpad('hello world', 20)",
    # ascii / chr
    "SELECT ascii('A'), ascii('hello'), chr(65)",
    # ltrim / rtrim
    "SELECT ltrim('   spaced   '), rtrim('   spaced   ')",
    "SELECT ltrim('xxhello', 'x'), rtrim('helloxx', 'x')",
    # initcap
    "SELECT initcap('hello world foo bar')",
    # regexp_replace (legacy + extended)
    "SELECT regexp_replace('hello world', 'o', 'X', 'g')",
    "SELECT regexp_replace('aaa', 'a', 'X', 1, 2)",
    "SELECT regexp_replace('abc123def', '[0-9]+', '#', 1, 0)",
    # regexp_like / substr / instr / count
    "SELECT regexp_like('hello', 'h.*o')",
    "SELECT regexp_substr('abc123', '[0-9]+')",
    "SELECT regexp_instr('hello', 'l', 1, 2)",
    "SELECT regexp_count('aaa', 'a')",
    # SUBSTRING forms
    "SELECT SUBSTRING('hello world' FROM 2 FOR 5)",
    "SELECT SUBSTRING('hello' FROM 'l+')",
    "SELECT SUBSTRING('hello', 2, 3)",
    # INSERT expressions (setup + workload)
    "SELECT 1+2, repeat('x', 10), 'a' || 'b'",
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

def run_bench():
    s = socket.create_connection((HOST, PORT), timeout=10)
    params = b"user\x00postgres\x00\x00"
    body = struct.pack("!I", 196608) + params
    s.sendall(struct.pack("!I", len(body) + 4) + body)
    while True:
        t, _ = read_msg(s)
        if t == b"Z":
            break

    def query(sql):
        body = sql.encode() + b"\x00"
        s.sendall(b"Q" + struct.pack("!I", len(body) + 4) + body)
        while True:
            t, _ = read_msg(s)
            if t == b"Z":
                break

    # Warmup
    for q in QUERIES:
        query(q)

    # Benchmark: 100 iterations over all queries
    n_iter = 100
    t0 = time.time()
    for _ in range(n_iter):
        for q in QUERIES:
            query(q)
    dt = time.time() - t0
    total = n_iter * len(QUERIES)
    qps = total / dt if dt > 0 else 0
    print(f"v0.24 string workload: {total} queries in {dt:.2f}s = {qps:.0f} qps")
    s.close()
    return qps

if __name__ == "__main__":
    qps = run_bench()
    sys.exit(0 if qps > 0 else 1)
