#!/usr/bin/env python3
"""cg_v098.py — callgrind/DHAT workload driver for the v0.98 sequence paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises the v0.98 hot paths:
  1. volatile-predicate WHERE over a 10k-row table (nextval per row,
     no pushdown)
  2. repeated nextval/currval/setval/lastval traffic
  3. pg_sequences + information_schema.sequences catalog scans
  4. CREATE/ALTER SEQUENCE churn (cache, owned_by, restart)
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

    # Setup: 10k-row table, sequences
    q("create table cg98t as select g % 100 as v from generate_series(1, 10000) g;")
    q("create sequence cg98s;")
    q("create sequence cg98c cache 100;")

    # 1. volatile WHERE: nextval evaluated per row, no pushdown
    for _ in range(N):
        q("select count(*) from (select distinct v from cg98t) ss where v < 100 + nextval('cg98s');")

    # 2. sequence function traffic
    for _ in range(N):
        q("select nextval('cg98s'), currval('cg98s'), lastval();")
        q("select setval('cg98s', 1000, false);")

    # 3. catalog scans
    for _ in range(N):
        q("select * from pg_sequences;")
        q("select sequence_name, cache_size from pg_sequences where cache_size > 1;")
        q("select * from information_schema.sequences;")

    # 4. DDL churn
    for i in range(N):
        q(f"create sequence cg98x{i} cache {1 + i % 50};")
        q(f"alter sequence cg98x{i} restart with {i + 1};")
        q(f"drop sequence cg98x{i};")

    q("select nextval('cg98c');")
    print(f"cg_v098: workload done (N={N})")


if __name__ == "__main__":
    main()
