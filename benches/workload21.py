#!/usr/bin/env python3
"""v0.21 float8-heavy workload benchmark.

Measures throughput of the float8 math paths added in v0.21:
trigonometric, hyperbolic, exponential/logarithmic functions, prefix
operators, and Infinity/NaN arithmetic.
"""
import socket
import struct
import sys
import time

HOST = "127.0.0.1"
PORT = 5433

QUERIES = [
    # trig / inverse trig
    "SELECT sin(1.0::float8), cos(1.0::float8), tan(1.0::float8)",
    "SELECT asin(0.5::float8), acos(0.5::float8), atan(1.0::float8)",
    "SELECT atan2(1.0::float8, 2.0::float8)",
    # hyperbolic
    "SELECT sinh(1.0::float8), cosh(1.0::float8), tanh(1.0::float8)",
    # exp / log / power
    "SELECT exp(1.0::float8), ln(2.718281828459045::float8)",
    "SELECT log(100.0::float8), power(2.0::float8, 10.0::float8)",
    "SELECT sqrt(2.0::float8), cbrt(27.0::float8)",
    "SELECT lgamma(5.0::float8), erf(0.5::float8), erfc(0.5::float8)",
    # rounding
    "SELECT trunc(1.9::float8), round(2.5::float8), ceil(2.1::float8)",
    "SELECT floor(2.9::float8), sign(-3.5::float8)",
    # prefix operators
    "SELECT @ (-2.5::float8), |/ 64::float8, ||/ 27::float8",
    # Infinity / NaN arithmetic
    "SELECT 'Infinity'::float8 + 100.0, 'nan'::float8 / '0'::float8",
    "SELECT 42::float8 / 'Infinity'::float8",
    # float8send
    "SELECT float8send(3.14159::float8)",
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
    sock.sendall(struct.pack("!I", len(params) + 8) + struct.pack("!I", 196608) + params)
    while True:
        typ, _ = read_msg(sock)
        if typ == b"Z":
            break

def simple_query(sock, q):
    msg = b"Q" + struct.pack("!I", len(q) + 5) + q.encode() + b"\x00"
    sock.sendall(msg)
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
