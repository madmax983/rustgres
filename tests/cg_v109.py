#!/usr/bin/env python3
"""cg_v109.py — callgrind workload driver for the v1.09 function DDL paths.

Assumes a rustgres server is already running under callgrind on port
5434; issues the v1.09 CREATE/ALTER FUNCTION + user-SRF workload
against it and exits. N=40 mirrors cg_v108.py.
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

    # 1. Setup: functions with PARALLEL options, SRF, volatility
    q("CREATE FUNCTION cg109a(int) RETURNS int LANGUAGE sql IMMUTABLE PARALLEL SAFE AS 'SELECT $1 * 2';")
    q("CREATE FUNCTION cg109b(int) RETURNS int LANGUAGE sql PARALLEL RESTRICTED AS 'SELECT $1 + 1';")
    q("CREATE FUNCTION cg109srf(int) RETURNS SETOF int AS 'SELECT generate_series(1, $1)' LANGUAGE sql IMMUTABLE;")
    q("CREATE FUNCTION cg109v(int) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT $1';")

    # 2. Workload: exercise v1.09 function paths N times
    for i in range(N):
        # PARALLEL-hint function calls
        q(f"SELECT cg109a({i});")
        q(f"SELECT cg109b({i});")
        # User SRF targetlist expansion
        q(f"SELECT cg109srf({i % 10 + 1});")
        q("SELECT cg109srf(3) ORDER BY 1;")
        # ALTER FUNCTION volatility
        q("ALTER FUNCTION cg109v(int) IMMUTABLE;")
        q("ALTER FUNCTION cg109v(int) STABLE;")
        q("ALTER FUNCTION cg109v(int) VOLATILE;")
        q(f"SELECT cg109v({i});")

    s.close()

if __name__ == "__main__":
    main()
