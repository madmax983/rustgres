#!/usr/bin/env python3
"""cg_v116.py — callgrind workload driver for the v1.16 partition paths.

Assumes a rustgres server is already running under callgrind on port
5436; issues the v1.16 partition workload against it and exits.
N=40 mirrors cg_v115.py.
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

    # 1. Setup: RANGE + LIST + multilevel
    q("CREATE TABLE cg116r (a int, b int) PARTITION BY RANGE (a);")
    q("CREATE TABLE cg116r1 PARTITION OF cg116r FOR VALUES FROM (0) TO (1000);")
    q("CREATE TABLE cg116r2 PARTITION OF cg116r FOR VALUES FROM (1000) TO (2000);")
    q("INSERT INTO cg116r SELECT g % 2000, g FROM generate_series(1, 200) g;")
    q("CREATE TABLE cg116m (a int, b int, c text) PARTITION BY RANGE (a, b);")
    q("CREATE TABLE cg116m5 PARTITION OF cg116m FOR VALUES FROM (1, 0) TO (1, 100)"
      " PARTITION BY RANGE (c);")
    q("CREATE TABLE cg116m5c PARTITION OF cg116m5 FOR VALUES FROM ('a') TO ('z');")
    q("INSERT INTO cg116m SELECT 1, g % 100, 'k' || (g % 20)"
      " FROM generate_series(1, 100) g;")

    # 2. Workload: exercise v1.16 partition paths N times
    for i in range(N):
        # routing inserts (RANGE + multilevel)
        q("INSERT INTO cg116r VALUES (500, 1), (1500, 2);")
        q("INSERT INTO cg116m VALUES (1, 42, 'm');")
        # parent scans
        q("SELECT * FROM cg116r ORDER BY a LIMIT 10;")
        q("SELECT count(*) FROM cg116m;")
        # leaf scans
        q("SELECT * FROM cg116r1 WHERE a < 100;")
        q("SELECT * FROM cg116m5c WHERE c = 'k5';")
        # v1.16 23514 error paths
        q("INSERT INTO cg116r VALUES (9999, 1);")
        q("INSERT INTO cg116m VALUES (1, 42, 'zz');")

    # 3. Teardown
    q("DROP TABLE cg116r;")
    q("DROP TABLE cg116m;")
    s.close()
    print(f"cg_v116: workload done (N={N})")


if __name__ == "__main__":
    main()
