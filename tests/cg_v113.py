#!/usr/bin/env python3
"""cg_v113.py — callgrind workload driver for the v1.13 tableoid paths.

Assumes a rustgres server is already running under callgrind on port
5435; issues the v1.13 tableoid workload against it and exits.
N=40 mirrors cg_v112.py.
"""
import os, socket, struct, sys

PORT = 5435
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

    # 1. Setup: partitioned table
    q("CREATE TABLE cg113t (a text, b int) PARTITION BY LIST (a);")
    q("CREATE TABLE cg113p1 PARTITION OF cg113t FOR VALUES IN ('x');")
    q("CREATE TABLE cg113p2 PARTITION OF cg113t FOR VALUES IN ('y');")
    q("INSERT INTO cg113t SELECT CASE WHEN g % 2 = 0 THEN 'x' ELSE 'y' END, g FROM generate_series(1, 50) g;")

    # 2. Workload: exercise v1.13 tableoid paths N times
    for i in range(N):
        # tableoid in select list, where, group by, order by
        q("SELECT tableoid::regclass, a, b FROM cg113t ORDER BY a, b;")
        q("SELECT tableoid FROM cg113t;")
        q("SELECT tableoid::regclass::text, count(*) FROM cg113t GROUP BY 1 ORDER BY 1;")
        q("SELECT a FROM cg113t WHERE tableoid::regclass = 'cg113p1';")
        q("SELECT tableoid::regclass AS p, b FROM cg113t ORDER BY p, b;")
        # pg_size_pretty
        q("SELECT pg_size_pretty(8192);")
        q("SELECT pg_size_pretty(pg_relation_size('cg113t'::regclass));")

    # 3. Teardown
    q("DROP TABLE cg112t;")
    s.close()
    print(f"cg_v112: workload done (N={N})")


if __name__ == "__main__":
    main()
