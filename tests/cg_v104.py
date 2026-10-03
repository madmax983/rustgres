#!/usr/bin/env python3
"""cg_v104.py — callgrind workload driver for the v1.04 multi-statement
simple-Query implicit-transaction paths.

Assumes a rustgres server is already running under callgrind on port
5434; issues the v1.04 multi-statement workload against it and exits.
N=40 mirrors cg_v103.py.
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

    # 1. Setup: a table for the multi-statement workload
    q("CREATE TABLE cg104(a int, b text);")
    q("INSERT INTO cg104 SELECT g, 'x' || g FROM generate_series(1,100) g;")

    # 2. Workload: exercise v1.04 implicit-block paths N times
    for i in range(N):
        # repeated result sets in one Q
        q(f"SELECT {i}; SELECT {i+1}; SELECT {i+2};")
        # mixed DML + read in one implicit block
        q(f"INSERT INTO cg104 VALUES ({1000+i}, 'n{i}'); "
          f"SELECT count(*) FROM cg104; DELETE FROM cg104 WHERE a = {1000+i};")
        # mid-Q error: abort the implicit block, skip the rest
        q("SELECT 1; SELECT 1/0; SELECT 3;")
        # COMMIT-in-block warning + fresh block
        q("SELECT 1; COMMIT; SELECT 2;")
        # BEGIN conversion, then end the explicit txn
        q("BEGIN; SELECT 1; COMMIT;")

    s.close()
    print(f"cg_v104: {N} iterations done")

if __name__ == "__main__":
    main()
