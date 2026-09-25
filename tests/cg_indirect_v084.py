#!/usr/bin/env python3
"""cg_indirect_v084.py — callgrind workload for v0.84 INSERT indirection.

Assumes a rustgres server already running on 5433 (under callgrind).
Blends: array indirection inserts, composite field inserts, nested
inserts, and selects over the results.
"""
import socket, struct

PORT = 5433

def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
    params = b"user\x00postgres\x00database\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
    while True:
        hdr = s.recv(5); typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]; s.recv(ln - 4)
        if typ == b"Z":
            break

    def simple(sql):
        s.sendall(b"Q" + struct.pack("!i", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        while True:
            hdr = s.recv(5); typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]; s.recv(ln - 4)
            if typ == b"Z":
                break

    simple("create type cg_tt as (a int, b text[])")
    simple("create table cg_ind (x int, arr int[], c cg_tt, carr cg_tt[])")
    for i in range(150):
        simple(f"insert into cg_ind (arr[1], arr[2]) values ({i},{i+1})")
    for i in range(100):
        simple(f"insert into cg_ind (c.a, c.b) values ({i}, '{{x{i}}}')")
    for i in range(100):
        simple(f"insert into cg_ind (c.b[1], c.b[2]) values ('p{i}','q{i}')")
    for i in range(50):
        simple(f"insert into cg_ind (carr[1].b[1], carr[1].b[2]) values ('m{i}','n{i}')")
    for i in range(50):
        simple("insert into cg_ind (arr[1], arr[2]) select 7,8")
    for _ in range(30):
        simple("select x, arr, c, carr from cg_ind")
    s.sendall(b"X" + struct.pack("!i", 4))
    s.close()

main()
