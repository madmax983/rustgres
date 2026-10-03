#!/usr/bin/env python3
"""cg_v115.py — callgrind workload driver for the v1.15 quote_* paths.

Assumes a rustgres server is already running under callgrind on port
5436; issues the v1.15 quote_* workload against it and exits.
N=40 mirrors cg_v114.py.
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
    q("CREATE TABLE cg115t (id int, s text);")
    q("INSERT INTO cg115t SELECT g, 'val' || g FROM generate_series(1, 50) g;")
    q("INSERT INTO cg115t VALUES (51, 'O''Brien'), (52, 'a\\b'), (53, NULL),"
      " (54, 'select'), (55, 'My Col');")

    # 2. Workload: exercise v1.15 quote_* paths N times
    for i in range(N):
        # quote_literal: table scan, literals, E'' path, strict NULL
        q("SELECT id, quote_literal(s) FROM cg115t;")
        q("SELECT quote_literal('it''s');")
        q(r"SELECT quote_literal('x\y');")
        q("SELECT quote_literal(NULL);")
        # quote_ident: table scan, keywords, unsafe shapes, strict NULL
        q("SELECT id, quote_ident(s) FROM cg115t;")
        q("SELECT quote_ident('select'), quote_ident('abort');")
        q("SELECT quote_ident('a\"b');")
        q("SELECT quote_ident(NULL);")
        # quote_nullable
        q("SELECT id, quote_nullable(s) FROM cg115t;")
        q("SELECT quote_nullable(NULL);")
        # error paths (validation)
        q("SELECT quote_literal('a', 'b');")
        q("SELECT quote_ident();")

    # 3. Teardown
    q("DROP TABLE cg115t;")
    s.close()
    print(f"cg_v115: workload done (N={N})")


if __name__ == "__main__":
    main()
