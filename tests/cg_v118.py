#!/usr/bin/env python3
"""cg_v118.py — callgrind workload driver for the v1.18 temp index-order paths.

Assumes a rustgres server is already running under callgrind on port
5436; issues the v1.18 temp-table ORDER BY workload against it and exits.
N=40 mirrors cg_v117.py.
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

    # 1. Setup: temp table + temp index (session-local, one connection)
    q("CREATE TEMP TABLE cg118t (f1 int);")
    q("INSERT INTO cg118t SELECT g % 97 FROM generate_series(1, 500) g;")
    q("INSERT INTO cg118t VALUES (NULL), (NULL);")
    q("CREATE INDEX cg118i ON cg118t (f1);")
    # permanent-table control
    q("CREATE TABLE cg118p (f1 int);")
    q("INSERT INTO cg118p SELECT g % 89 FROM generate_series(1, 500) g;")
    q("CREATE INDEX cg118pi ON cg118p (f1);")

    # 2. Workload: exercise v1.18 temp index-order paths N times
    for i in range(N):
        # the conformance shape: temp ORDER BY via OrderHint
        q("SELECT * FROM cg118t ORDER BY f1;")
        q("SELECT * FROM cg118t ORDER BY f1 DESC;")
        # early-limit index-order path
        q("SELECT * FROM cg118t ORDER BY f1 LIMIT 10;")
        q("SELECT * FROM cg118t ORDER BY f1 DESC LIMIT 10;")
        # hint declines (NULLS FIRST on ASC index) -> sort fallback
        q("SELECT * FROM cg118t ORDER BY f1 NULLS FIRST;")
        # permanent-table control through the global index map
        q("SELECT * FROM cg118p ORDER BY f1;")
        q("SELECT * FROM cg118p ORDER BY f1 DESC LIMIT 10;")

    # 3. Teardown
    q("DROP TABLE cg118p;")
    s.close()
    print(f"cg_v118: workload done (N={N})")


if __name__ == "__main__":
    main()
