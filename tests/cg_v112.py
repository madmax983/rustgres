#!/usr/bin/env python3
"""cg_v112.py — callgrind workload driver for the v1.12 zero-target-list paths.

Assumes a rustgres server is already running under callgrind on port
5435; issues the v1.12 empty-select workload against it and exits.
N=40 mirrors cg_v111.py.
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

    # 1. Setup: base table
    q("CREATE TABLE cg112t (a int, b text);")
    q("INSERT INTO cg112t SELECT g, 'x' FROM generate_series(1, 50) g;")

    # 2. Workload: exercise v1.12 zero-target-list paths N times
    for i in range(N):
        # bare / FROM-attached / WHERE-only empty selects
        q("SELECT;")
        q("SELECT FROM cg112t;")
        q("SELECT FROM cg112t WHERE a > 10;")
        q("SELECT WHERE false;")
        q("SELECT FROM generate_series(1, 20);")
        q("SELECT DISTINCT FROM generate_series(1, 20);")
        # zero-column set operations (all variants)
        q("SELECT UNION SELECT;")
        q("SELECT INTERSECT SELECT;")
        q("SELECT EXCEPT SELECT;")
        q("SELECT FROM generate_series(1, 5) UNION ALL SELECT FROM generate_series(1, 3);")
        q("SELECT FROM generate_series(1, 5) UNION SELECT FROM generate_series(1, 3);")
        q("SELECT FROM generate_series(1, 5) INTERSECT ALL SELECT FROM generate_series(1, 3);")
        q("SELECT FROM generate_series(1, 5) EXCEPT ALL SELECT FROM generate_series(1, 3);")
        # CTE variants
        q("WITH cte AS MATERIALIZED (SELECT s FROM generate_series(1, 5) s) "
          "SELECT FROM cte UNION SELECT FROM cte;")
        q("WITH cte AS NOT MATERIALIZED (SELECT s FROM generate_series(1, 5) s) "
          "SELECT FROM cte UNION SELECT FROM cte;")
        # error path (parse reject, no executor work)
        q("SELECT FROM;")

    # 3. Teardown
    q("DROP TABLE cg112t;")
    s.close()
    print(f"cg_v112: workload done (N={N})")


if __name__ == "__main__":
    main()
