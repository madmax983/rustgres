#!/usr/bin/env python3
"""cg_v103.py — callgrind workload driver for the v1.03 DECLARE/:=/FOR/RETURN NEXT/SETOF/EXPLAIN ANALYZE paths.

Assumes a rustgres server is already running under callgrind on port
5434; issues the v1.03 workload against it and exits. N=40 mirrors
cg_v102.py.
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

    # 1. Setup: functions exercising the v1.03 paths
    q("CREATE TABLE cg_sq (pk int primary key, c1 int, c2 int);")
    q("INSERT INTO cg_sq SELECT g, g%4, g%4 FROM generate_series(1,100) g;")
    q("""CREATE FUNCTION cg_add(a int, b int) RETURNS int AS $$
declare s int;
begin
  s := a + b;
  return s;
end;
$$ LANGUAGE plpgsql""")
    q("""CREATE FUNCTION cg_gen(n int) RETURNS SETOF int AS $$
declare i int;
begin
  for i in select * from generate_series(1, n) loop
    return next i * 2;
  end loop;
end;
$$ LANGUAGE plpgsql""")
    q("""CREATE FUNCTION cg_expl() RETURNS SETOF text LANGUAGE plpgsql AS $$
declare ln text;
begin
    for ln in
        explain (analyze, summary off, timing off, costs off, buffers off)
        select * from (select pk,c2 from cg_sq order by c1,pk) as x limit 3
    loop
        return next ln;
    end loop;
end;
$$""")

    # 2. Workload: exercise v1.03 paths N times
    for i in range(N):
        q(f"SELECT cg_add({i}, {i+1});")
        q(f"SELECT * FROM cg_gen(10);")
        q("SELECT * FROM cg_expl();")
        q("EXPLAIN (ANALYZE) SELECT * FROM cg_sq WHERE c1 = 1;")

    s.close()
    print(f"cg_v103: {N} iterations done")

if __name__ == "__main__":
    main()
