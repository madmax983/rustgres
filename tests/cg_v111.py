#!/usr/bin/env python3
"""cg_v111.py — callgrind workload driver for the v1.11 composite-value paths.

Assumes a rustgres server is already running under callgrind on port
5434; issues the v1.11 ARRAY[...]/ROW(...) UNION and row-subquery
workload against it and exits. N=40 mirrors cg_v110.py.
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
    q("CREATE TABLE cg111t (a int, b int);")
    q("INSERT INTO cg111t SELECT g, g * 10 FROM generate_series(1, 50) g;")

    # 2. Workload: exercise v1.11 composite-value paths N times
    for i in range(N):
        # array UNION / INTERSECT / EXCEPT (hint_type + value_key)
        q("select x from (values (array[1, 2]), (array[1, 3])) _(x) union "
          "select x from (values (array[1, 2]), (array[1, 4])) _(x);")
        q("select x from (values (array[1, 2]), (array[1, 3])) _(x) intersect "
          "select x from (values (array[1, 2]), (array[1, 4])) _(x);")
        q("select x from (values (array[1, 2]), (array[1, 3])) _(x) except "
          "select x from (values (array[1, 2]), (array[1, 4])) _(x);")
        # record UNION / INTERSECT / EXCEPT
        q("select x from (values (row(1, 2)), (row(1, 3))) _(x) union "
          "select x from (values (row(1, 2)), (row(1, 4))) _(x);")
        q("select x from (values (row(1, 2)), (row(1, 3))) _(x) intersect "
          "select x from (values (row(1, 2)), (row(1, 4))) _(x);")
        # row-valued subqueries (correlated, uncorrelated, error path)
        q("SELECT ROW(1, 2) = (SELECT a, b) AS eq FROM cg111t;")
        q("SELECT ROW(1, 2) = (SELECT 3, 4) AS eq FROM cg111t;")
        q("SELECT (SELECT a, b FROM cg111t LIMIT 1) = ROW(1, 2);")
        # array subscript + ANY
        q("SELECT (SELECT ARRAY[1,2,3])[1];")
        q("select * from (values (1, array[10,20])) as v(x, ys) where x = any (array[1,2]);")

    # 3. Teardown
    q("DROP TABLE cg111t;")
    s.close()
    print(f"cg_v111: workload done (N={N})")


if __name__ == "__main__":
    main()
