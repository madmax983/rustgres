#!/usr/bin/env python3
"""cg_v110.py — callgrind workload driver for the v1.10 LATERAL paths.

Assumes a rustgres server is already running under callgrind on port
5434; issues the v1.10 LATERAL (SELECT/VALUES) workload against it and
exits. N=40 mirrors cg_v109.py.
"""
import os, socket, struct, sys

PORT = 5434
N = int(sys.argv[1]) if len(sys.argv) > 1 else 40

def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    def rd(n):
        d = b""
        while len(d) < n:
            c = s.recv(n - len(d))
            if not c: raise RuntimeError("closed")
            d += c
        return d
    def msg():
        t = rd(1); ln = struct.unpack("!i", rd(4))[0]
        return t, rd(ln - 4)
    while True:
        t, _ = msg()
        if t == b"Z": break
    def q(sql):
        s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        while True:
            t, _ = msg()
            if t == b"Z": break

    # 1. Setup: base table
    q("CREATE TABLE cg110t (a int, b int);")
    q("INSERT INTO cg110t SELECT g, g * 10 FROM generate_series(1, 50) g;")

    # 2. Workload: exercise v1.10 LATERAL paths N times
    for i in range(N):
        # comma LATERAL (SELECT ...) correlated
        q("SELECT t.a, s.y FROM cg110t t, LATERAL (SELECT t.a + t.b AS y) AS s;")
        # INNER JOIN LATERAL with ON
        q("SELECT t.a, s.y FROM cg110t t INNER JOIN LATERAL (SELECT t.b * 2 AS y) AS s ON s.y > t.a;")
        # LEFT JOIN LATERAL null-extension
        q("SELECT t.a, s.y FROM cg110t t LEFT JOIN LATERAL (SELECT t.b AS y WHERE t.b > 10000) AS s ON true;")
        # LATERAL (VALUES ...) correlated
        q("SELECT t.a, v.x FROM cg110t t, LATERAL (VALUES (t.a * 10), (t.b)) AS v(x);")
        # uncorrelated LATERAL
        q("SELECT t.a, s.k FROM cg110t t, LATERAL (SELECT 7 AS k) AS s WHERE t.a < 5;")
        # nested LATERAL
        q("SELECT t.a, s1.x, s2.y FROM cg110t t, LATERAL (SELECT t.a + 1 AS x) AS s1, LATERAL (SELECT s1.x + 1 AS y) AS s2 WHERE t.a < 5;")
        # multi-row correlated fan-out
        q("SELECT t.a, s.z FROM cg110t t, LATERAL (SELECT t.a AS z UNION ALL SELECT t.b) AS s WHERE t.a < 5;")

    q("DROP TABLE cg110t;")


main()
