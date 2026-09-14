#!/usr/bin/env python3
"""rustgres v0.25 benchmark: integer input parsing, bitwise operators,
numeric edge cases.

Measures throughput (queries/sec) for v0.25 workloads. Run with a
rustgres server on port 5433.

Run: python3 benches/workload25.py
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


def bench(s, rd, name, queries, n=1000):
    # Warmup
    for q in queries[:10]:
        sql(s, rd, q)
    start = time.perf_counter()
    for _ in range(n):
        for q in queries:
            sql(s, rd, q)
    elapsed = time.perf_counter() - start
    total = n * len(queries)
    qps = total / elapsed
    print(f"{name}: {qps:.0f} qps ({total} queries in {elapsed:.2f}s)")
    return qps


def main():
    s, rd = connect()

    # v0.25: integer input parsing (non-decimal, underscores)
    bench(s, rd, "int_input_hex", [
        "SELECT int8 '0x42F'",
        "SELECT int8 '0x7FFFFFFFFFFFFFFF'",
        "SELECT int4 '0x1EEE_FFFF'",
    ])

    bench(s, rd, "int_input_bin_oct", [
        "SELECT int8 '0b100101'",
        "SELECT int8 '0o273'",
        "SELECT int8 '0b_10_0101'",
    ])

    bench(s, rd, "int_input_underscore", [
        "SELECT int8 '1_000_000'",
        "SELECT '12_000_000_000'::numeric",
        "SELECT '23_000_000_000e-1_0'::numeric",
    ])

    # v0.25: bitwise operators
    bench(s, rd, "bitwise", [
        "SELECT 123 & 456",
        "SELECT 123 | 456",
        "SELECT 123 # 456",
        "SELECT ~123",
        "SELECT 5 << 2",
        "SELECT 20 >> 2",
    ])

    bench(s, rd, "bitwise_int8", [
        "SELECT 9223372036854775807::int8 & 255",
        "SELECT ~9223372036854775807::int8",
        "SELECT (-1::int8 << 63)::text",
    ])

    # v0.25: numeric edge cases
    bench(s, rd, "numeric_special", [
        "SELECT 'NaN'::float8::numeric",
        "SELECT sqrt('inf'::numeric)",
        "SELECT div('nan'::numeric, '0')",
        "SELECT width_bucket('NaN', 3.0, 4.0, 888)",
    ])

    bench(s, rd, "power_gcd", [
        "SELECT power('-2'::numeric, '3')",
        "SELECT gcd(123456::numeric, 789::numeric)",
        "SELECT lcm(12::int4, 18::int4)",
    ])

    s.close()


if __name__ == "__main__":
    main()
