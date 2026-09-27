#!/usr/bin/env python3
"""cg_v108.py — callgrind workload driver for the v1.08 EXPLAIN PG-text paths.

Assumes a rustgres server is already running under callgrind on port
5434; issues the v1.08 EXPLAIN (COSTS OFF) workload against it and exits.
N=40 mirrors cg_v104.py.
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

    # 1. Setup
    q("CREATE TABLE cg108a(a int, b text);")
    q("CREATE TABLE cg108b(x int, y int);")
    q("CREATE INDEX cg108ai ON cg108a(a);")
    q("INSERT INTO cg108a SELECT g, 'v'||g FROM generate_series(1,200) g;")
    q("INSERT INTO cg108b SELECT g, g*2 FROM generate_series(1,100) g;")

    # 2. Workload: exercise v1.08 PG-text EXPLAIN paths N times
    for i in range(N):
        # seq scan + filter deparse
        q("EXPLAIN (COSTS OFF) SELECT * FROM cg108a WHERE b = 'v7';")
        # index scan: index cond + residual filter
        q(f"EXPLAIN (COSTS OFF) SELECT * FROM cg108a WHERE a = {i} AND b = 'v3';")
        # range cond
        q("EXPLAIN (COSTS OFF) SELECT * FROM cg108a WHERE a > 10 AND a < 190;")
        # SIMILAR TO deparse
        q("EXPLAIN (COSTS OFF) SELECT * FROM cg108a WHERE b SIMILAR TO 'v[0-9]+';")
        # join: ON -> Join Filter, WHERE split between inputs
        q("EXPLAIN (COSTS OFF) SELECT * FROM cg108a a, cg108b b WHERE a.a = b.x AND a.a > 0;")
        # left join (best-effort, masked)
        q("EXPLAIN (COSTS OFF) SELECT * FROM cg108a LEFT JOIN cg108b ON a = x;")
        # sort + limit
        q("EXPLAIN (COSTS OFF) SELECT * FROM cg108a ORDER BY a DESC LIMIT 7;")
        # aggregate
        q("EXPLAIN (COSTS OFF) SELECT count(*), sum(a) FROM cg108a GROUP BY b;")
        # CTE + subquery
        q("EXPLAIN (COSTS OFF) WITH w AS (SELECT * FROM cg108a WHERE a < 50) SELECT * FROM w WHERE w.a > 5;")
        # option parsing: duplicates last-wins, inert options
        q("EXPLAIN (VERBOSE, COSTS OFF, COSTS ON, COSTS OFF) SELECT * FROM cg108a;")
        # error paths (parse-level)
        q("EXPLAIN (FOOBAR) SELECT 1;")
        q("EXPLAIN (COSTS frobnicate) SELECT 1;")
        # COSTS ON legacy path
        q("EXPLAIN SELECT * FROM cg108a WHERE a = 1;")
        # ANALYZE + COSTS OFF (row-producing)
        q("EXPLAIN (ANALYZE, COSTS OFF) SELECT * FROM cg108a WHERE a < 20;")

    s.close()
    print(f"cg_v108: {N} iterations done")

if __name__ == "__main__":
    main()
