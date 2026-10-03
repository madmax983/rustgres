#!/usr/bin/env python3
"""cg_v117.py — callgrind workload driver for the v1.17 xmin/xmax paths.

Assumes a rustgres server is already running under callgrind on port
5436; issues the v1.17 xmin/xmax workload against it and exits.
N=40 mirrors cg_v116.py.
"""
import os, socket, struct, sys

PORT = 5436
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
    q("CREATE TABLE cg117t (a int, b text);")
    q("INSERT INTO cg117t SELECT g, 'v' || g FROM generate_series(1, 200) g;")
    q("CREATE TABLE cg117o (x int);")
    q("INSERT INTO cg117o SELECT g FROM generate_series(1, 50) g;")

    # 2. Workload: exercise v1.17 xmin/xmax paths N times
    for i in range(N):
        # qualified self-joins (the conformance shape)
        q("SELECT a.xmin = b.xmin FROM cg117t a, cg117t b WHERE a.a=1 AND b.a=2;")
        q("SELECT a.xmax = b.xmax FROM cg117t a, cg117t b WHERE a.a=3 AND b.a=4;")
        # xmin/xmax projection with provenance
        q("SELECT xmin, xmax FROM cg117t ORDER BY a LIMIT 10;")
        # qualified across different tables
        q("SELECT a.xmin, o.xmin FROM cg117t a, cg117o o WHERE a.a=5 AND o.x=5;")
        # xmin in WHERE / ORDER BY / GROUP BY
        q("SELECT a FROM cg117t WHERE xmin > 0 ORDER BY a LIMIT 5;")
        q("SELECT xmin, count(*) FROM cg117t GROUP BY xmin;")
        # 42702 ambiguity path
        q("SELECT xmin FROM cg117t a, cg117t b LIMIT 1;")
        # DML churn (new row versions)
        q("INSERT INTO cg117t VALUES (1000, 'new');")
        q("DELETE FROM cg117t WHERE a = 1000;")

    # 3. Teardown
    q("DROP TABLE cg117t;")
    q("DROP TABLE cg117o;")
    s.close()
    print(f"cg_v117: workload done (N={N})")


if __name__ == "__main__":
    main()
