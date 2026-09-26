#!/usr/bin/env python3
"""cg_v102.py — callgrind workload driver for the v1.02 RAISE NOTICE /
RAISE EXCEPTION paths.

Assumes a rustgres server is already running under callgrind on port
5434 (see v101 pattern); issues the v1.02 workload against it and
exits. N=40 mirrors cg_v101.py."""
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

    # 1. Setup: functions exercising the v1.02 paths
    q("CREATE TABLE cgt2 (x int, y int);")
    q("""CREATE FUNCTION cg_tattle(x int, y int) RETURNS bool AS $$
BEGIN
  RAISE NOTICE 'x = %, y = %', x, y;
  RETURN x > y;
END$$ LANGUAGE plpgsql VOLATILE;""")
    q("""CREATE FUNCTION cg_multi(a int, b text) RETURNS int AS $$
BEGIN
  RAISE NOTICE '100%% of %', a;
  RAISE NOTICE 'pair: %, %', a, b;
  RETURN a;
END$$ LANGUAGE plpgsql VOLATILE;""")
    q("""CREATE FUNCTION cg_boom() RETURNS int AS $$
BEGIN
  RAISE EXCEPTION 'kaput %', 42;
  RETURN 1;
END$$ LANGUAGE plpgsql VOLATILE;""")
    q("""CREATE FUNCTION cg_trap() RETURNS int AS $$
BEGIN
  RAISE NOTICE 'before';
  RAISE EXCEPTION 'nope';
  RETURN 1;
EXCEPTION WHEN raise_exception THEN
  RAISE NOTICE 'caught';
  RETURN -1;
END$$ LANGUAGE plpgsql VOLATILE;""")

    # 2. Repeated notice-emitting calls (notice sink + format path)
    for i in range(N):
        q(f"SELECT cg_tattle({i}, {i % 7});")
        q(f"SELECT cg_multi({i}, 's{i}');")
        q("SELECT cg_trap();")

    # 3. tattle in a WHERE clause (the conformance shape)
    for i in range(N // 2):
        q(f"INSERT INTO cgt2 VALUES ({i}, {i % 7});")
    for i in range(N // 4):
        q("SELECT x FROM cgt2 WHERE cg_tattle(x, y) ORDER BY 1 LIMIT 5;")

    # 4. Untrapped exception propagation
    for i in range(N // 4):
        q("SELECT cg_boom();")

    s.close()
    print(f"cg_v102 workload done (N={N})")

if __name__ == "__main__":
    main()
