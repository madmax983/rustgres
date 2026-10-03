#!/usr/bin/env python3
"""cg_v094.py — callgrind workload driver for the v0.94 PG19-parity paths.

Assumes a rustgres server already running (under callgrind) on 5434.
Exercises the v0.94 canonicalization hot paths:
  1. int hash join (baseline, no canonicalization)
  2. numeric cross-scale hash join (Numeric::hash_key: BigUint magnitude
     + trailing-zero stripping per key)
  3. float hash join with -0.0 / NaN (canon_float_key)
  4. GROUP BY / DISTINCT on numerics and floats (value_key_numeric)
  5. ORDER BY floats with NaN (pg_float_ord)
"""
import socket, struct

PORT = 5434


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    # drain startup
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

    # 1. int hash join baseline
    q("create table cgi(id int);")
    q("create table cgi2(id int);")
    q("insert into cgi select g from generate_series(1,3000) g;")
    q("insert into cgi2 select g from generate_series(1,3000) g;")
    q("select count(*) from cgi a join cgi2 b on a.id = b.id;")
    # 2. numeric cross-scale hash join
    q("create table cgn(id numeric);")
    q("create table cgn2(id numeric);")
    q("insert into cgn select (g || '.00')::numeric from generate_series(1,3000) g;")
    q("insert into cgn2 select g::numeric from generate_series(1,3000) g;")
    q("select count(*) from cgn a join cgn2 b on a.id = b.id;")
    # 3. float hash join with -0.0 / NaN
    q("create table cgf(id float8);")
    q("create table cgf2(id float8);")
    q("insert into cgf select case when g % 700 = 0 then 'NaN'::float8 when g % 500 = 0 then -0.0 else g::float8 end from generate_series(1,3000) g;")
    q("insert into cgf2 select case when g % 700 = 0 then 'NaN'::float8 when g % 500 = 0 then 0.0 else g::float8 end from generate_series(1,3000) g;")
    q("select count(*) from cgf a join cgf2 b on a.id = b.id;")
    # 4. GROUP BY / DISTINCT canonicalization
    q("select count(*) from (select distinct id from cgn) t;")
    q("select count(*) from (select distinct id from cgf) t;")
    q("select id, count(*) from cgn group by id order by 1 limit 5;")
    # 5. ORDER BY floats with NaN
    q("select id from cgf order by id limit 5;")
    q("select id from cgf order by id desc limit 5;")
    # scalar NaN comparisons
    q("select count(*) from cgf where id = id;")
    q("select count(*) from cgn where id = id;")
    s.close()
    print("cg_v094 workload done")


if __name__ == "__main__":
    main()
