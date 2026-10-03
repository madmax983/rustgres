#!/usr/bin/env python3
"""cg_v101.py — callgrind workload driver for the v1.01 plpgsql EXCEPTION paths.

Assumes a rustgres server already running (under callgrind) on 5434.
Exercises:
  1. CREATE FUNCTION with statement sequences + EXCEPTION/WHEN blocks
  2. Repeated trapped calls (division_by_zero -> handler) — stresses
     run_plpgsql_body / handler matching
  3. Untrapped error propagation
  4. Category / OTHERS / SQLSTATE handlers, named-arg rewrite
"""
import socket, struct, sys

PORT = 5434
N = int(sys.argv[1]) if len(sys.argv) > 1 else 40


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    while True:
        h = s.recv(5)
        t, ln = h[0:1], struct.unpack("!I", h[1:5])[0]
        b = b""
        while len(b) < ln - 4:
            b += s.recv(ln - 4 - len(b))
        if t == b"Z":
            break

    def q(sql):
        s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        while True:
            h = s.recv(5)
            if len(h) < 5:
                break
            t, ln = h[0:1], struct.unpack("!I", h[1:5])[0]
            b = b""
            while len(b) < ln - 4:
                chunk = s.recv(ln - 4 - len(b))
                if not chunk:
                    break
                b += chunk
            if t == b"Z":
                break

    # 1. DDL: exception-capable functions
    q("CREATE TABLE cgt (c float8);")
    q("""CREATE FUNCTION cg_inv(int) RETURNS float8 AS $$
BEGIN
  ANALYZE cgt;
  RETURN 1::float8/$1;
EXCEPTION
  WHEN division_by_zero THEN RETURN 0;
END$$ LANGUAGE plpgsql VOLATILE;""")
    q("CREATE FUNCTION cg_cat() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN data_exception THEN RETURN 7; END' LANGUAGE plpgsql;")
    q("CREATE FUNCTION cg_oth() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN OTHERS THEN RETURN 9; END' LANGUAGE plpgsql;")
    q("CREATE FUNCTION cg_named(x int) RETURNS int AS 'BEGIN RETURN x + 1; EXCEPTION WHEN OTHERS THEN RETURN 0; END' LANGUAGE plpgsql;")
    q("CREATE FUNCTION cg_first() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN division_by_zero THEN RETURN 1; WHEN OTHERS THEN RETURN 2; END' LANGUAGE plpgsql;")

    # 2. Repeated trapped calls (handler path)
    for i in range(N):
        q("SELECT cg_inv(0);")
        q(f"SELECT cg_inv({i + 1});")
        q("SELECT cg_cat();")
        q("SELECT cg_oth();")
        q("SELECT cg_first();")
        q(f"SELECT cg_named({i});")

    # 3. Inserts through the trapping function
    for i in range(N):
        q(f"INSERT INTO cgt VALUES (cg_inv({i % 3}));")

    # 4. Untrapped error propagation
    for i in range(N // 4):
        q("SELECT cg_named(1/0);")

    s.close()
    print(f"cg_v101 workload done (N={N})")

if __name__ == "__main__":
    main()
