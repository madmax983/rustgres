#!/usr/bin/env python3
"""cg_v099.py — callgrind/DHAT workload driver for the v0.99 paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises:
  1. typed-sequence DDL churn (AS smallint/int/bigint, ALTER AS, bounds)
  2. pg_sequences.cycle + information_schema catalog scans
  3. correlated derived-table queries (outer_schemas threading)
  4. parse_ident as table function
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

    q("create table cg99t(q1 int, q2 int);")
    q("insert into cg99t select g, g from generate_series(1, 1000) g;")

    # 1. typed-sequence DDL churn
    for i in range(N):
        tp = ("smallint", "int", "bigint")[i % 3]
        q(f"create sequence cg99s{i} as {tp};")
        q(f"alter sequence cg99s{i} as int;")
        q(f"select nextval('cg99s{i}');")
        q(f"drop sequence cg99s{i};")

    # 2. catalog scans
    for _ in range(N):
        q("select * from pg_sequences;")
        q("select sequencename, cycle from pg_sequences;")
        q("select sequence_name, cycle_option from information_schema.sequences;")

    # 3. correlated derived tables
    for _ in range(N):
        q("select *, (select r from (select q1 as q2) x, (select q2 as r) y) from cg99t;")

    # 4. parse_ident table function
    for _ in range(N):
        q("select * from parse_ident('\"Test\".col');")

    print(f"cg_v099: workload done (N={N})")


if __name__ == "__main__":
    main()
