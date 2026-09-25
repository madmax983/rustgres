#!/usr/bin/env python3
"""cg_v096.py — callgrind/DHAT workload driver for the v0.96 inheritance paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises the v0.96 hot paths:
  1. CREATE TABLE ... INHERITS (column merge across parents)
  2. ALTER TABLE ... INHERIT / NO INHERIT (compatibility checks)
  3. recursive parent scans (inheritance_descendants + row remap)
  4. FROM ONLY scans
  5. pg_inherits catalog scan
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

    # Setup: parent/child/grandchild hierarchy with data
    q("create table cgp (a int, b text);")
    q("create table cgc (c int) inherits (cgp);")
    q("create table cgg (d int) inherits (cgc);")
    q("insert into cgp select i, 'p' || i from generate_series(1, 200) i;")
    q("insert into cgc select i, 'c' || i, i * 10 from generate_series(1, 200) i;")
    q("insert into cgg select i, 'g' || i, i * 10, i * 100 from generate_series(1, 200) i;")

    for _ in range(N):
        # 1. CREATE ... INHERITS merge
        q("create temp table cg_tmp (z int) inherits (cgp);")
        q("drop table cg_tmp;")
        # 2. ALTER INHERIT / NO INHERIT
        q("create temp table cg_alt (b text, a int);")
        q("alter table cg_alt inherit cgp;")
        q("alter table cg_alt no inherit cgp;")
        q("drop table cg_alt;")
        # 3. recursive parent scan (parent + child + grandchild)
        q("select a, b from cgp order by a limit 50;")
        q("select count(*) from cgp;")
        # 4. ONLY scan
        q("select a, b from only cgp order by a limit 50;")
        # 5. pg_inherits
        q("select inhrelid, inhparent, inhseqno from pg_inherits;")
    s.close()
    print(f"workload done: {N} iterations")


if __name__ == "__main__":
    main()
