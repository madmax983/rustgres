#!/usr/bin/env python3
"""cg_v097.py — callgrind/DHAT workload driver for the v0.97 domain paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises the v0.97 hot paths:
  1. ALTER DOMAIN ADD/DROP CONSTRAINT (catalog clone + rewrite)
  2. domain CHECK enforcement on casts (check_domain_value)
  3. pg_typeof on domain casts and domain columns (static resolution)
  4. bounded plpgsql CREATE (desugar) + calls (SQL-body path)
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

    # Setup: domain + table with rows
    q("create domain cgdom as int check (value > 0);")
    q("create table cgt (a cgdom, b int);")
    q("insert into cgt select i, i from generate_series(1, 200) i;")
    q("create function cgvol(text) returns text as 'begin return $1; end' language plpgsql volatile;")

    for _ in range(N):
        # 1. ALTER DOMAIN ADD/DROP CONSTRAINT
        q("alter domain cgdom add constraint c_cg check (value < 1000000);")
        q("alter domain cgdom drop constraint c_cg;")
        # 2. domain CHECK enforcement on casts
        q("select i::cgdom from generate_series(1, 50) i;")
        # 3. pg_typeof: domain cast + domain column
        q("select pg_typeof(i::cgdom) from generate_series(1, 50) i;")
        q("select pg_typeof(a) from cgt limit 50;")
        # 4. plpgsql call
        q("select cgvol('x' || i) from generate_series(1, 50) i;")
    s.close()
    print(f"workload done: {N} iterations")


if __name__ == "__main__":
    main()
