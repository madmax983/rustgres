#!/usr/bin/env python3
"""rustgres v0.26 benchmark: numeric to_char / to_number formatting.

Measures throughput (queries/sec) for v0.26 formatting workloads.
Run with a rustgres server on port 5433.

Run: python3 benches/workload26.py
"""

import socket
import struct
import time

PORT = 5433
HOST = "127.0.0.1"


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
    while True:
        t = rd(1)
        ln = struct.unpack("!i", rd(4))[0]
        rd(ln - 4)
        if t == b"Z":
            break


QUERIES = [
    "SELECT to_char(12345.678::numeric, '9G999G999D99')",
    "SELECT to_char(-9876.543::numeric, 'MI999999.99')",
    "SELECT to_char(1234::numeric, 'FM999999')",
    "SELECT to_char(42::numeric, 'RN')",
    "SELECT to_char(12345.678::numeric, '9.999EEEE')",
    "SELECT to_char(1234.56::numeric, '99999V99')",
    "SELECT to_number('-34,338,492.654', '99G999G999D999')",
    "SELECT to_number('$1,234.56', 'L99,999.99')",
    "SELECT to_number('XIV', 'RN')",
    "SELECT to_char(123::int4, '09999')",
]


def main():
    s, rd = connect()
    # Warmup.
    for _ in range(200):
        for q in QUERIES:
            sql(s, rd, q)
    n = 0
    start = time.perf_counter()
    deadline = start + 5.0
    while time.perf_counter() < deadline:
        for q in QUERIES:
            sql(s, rd, q)
            n += 1
    elapsed = time.perf_counter() - start
    print(f"v0.26 formatting workload: {n} queries in {elapsed:.2f}s = {n/elapsed:.0f} qps")


if __name__ == "__main__":
    main()
