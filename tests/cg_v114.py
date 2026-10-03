#!/usr/bin/env python3
"""cg_v114.py — callgrind workload driver for the v1.14 FOR UPDATE OF paths.

Assumes a rustgres server is already running under callgrind on port
5436; issues the v1.14 FOR UPDATE OF workload against it and exits.
N=40 mirrors cg_v113.py.
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
    q("CREATE TABLE cg114t1 (a int, b int);")
    q("CREATE TABLE cg114t2 (a int, c int);")
    q("INSERT INTO cg114t1 SELECT g, g*10 FROM generate_series(1, 50) g;")
    q("INSERT INTO cg114t2 SELECT g, g*100 FROM generate_series(1, 50) g;")

    # 2. Workload: exercise v1.14 FOR UPDATE OF paths N times
    for i in range(N):
        # OF single table, alias, multi-target, subquery alias
        q("SELECT cg114t1.a FROM cg114t1, cg114t2 WHERE cg114t1.a = cg114t2.a FOR UPDATE OF cg114t1;")
        q("SELECT x.a FROM cg114t1 AS x FOR UPDATE OF x;")
        q("SELECT cg114t1.a FROM cg114t1, cg114t2 FOR UPDATE OF cg114t1, cg114t2;")
        q("SELECT * FROM (SELECT a FROM cg114t1) AS sq, cg114t2 FOR UPDATE OF sq;")
        # bare FOR UPDATE
        q("SELECT a FROM cg114t1 FOR UPDATE;")
        # error paths (validation)
        q("SELECT * FROM cg114t1 JOIN cg114t2 USING (a) AS j FOR UPDATE OF j;")
        q("SELECT * FROM cg114t1 FOR UPDATE OF nope;")

    # 3. Teardown
    q("DROP TABLE cg114t1;")
    q("DROP TABLE cg114t2;")
    s.close()
    print(f"cg_v114: workload done (N={N})")


if __name__ == "__main__":
    main()
