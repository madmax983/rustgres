#!/usr/bin/env python3
"""cg_v092.py — callgrind driver for the v0.92 array_agg + ORDER BY-in-aggregate
paths, plus the v0.90 Knuth division hot path.

Assumes a rustgres server already running (under callgrind) on 5434.
Exercises:
  1. array_agg with NULLs kept (scalar), DISTINCT, grouped, windowed
  2. array_agg(anyarray) accumulation incl. error paths
  3. ORDER BY in aggregates: array_agg ASC/DESC/multi-key, string_agg,
     sum, grouped ORDER BY, DISTINCT + ORDER BY
  4. Knuth div_rem via numeric division / ln / sqrt
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

    # 1. array_agg NULL-keeping
    q("select array_agg(i) from generate_series(1,2000) g(i)")
    q("select array_agg(case when i % 3 = 0 then null else i end) from generate_series(1,2000) g(i)")
    q("select array_agg(distinct i % 50) from generate_series(1,2000) g(i)")
    q("select i % 10, array_agg(i) from generate_series(1,1000) g(i) group by 1")
    q("select array_agg(i) over () from generate_series(1,200) g(i)")
    # 2. array_agg(anyarray)
    q("select array_agg(array[i, i+1]) from generate_series(1,200) g(i)")
    q("select array_agg(a) from (values (array[1,2]),(null)) v(a)")
    # 3. ORDER BY in aggregates (sort hot path)
    for _ in range(3):
        q("select array_agg(i order by i desc) from generate_series(1,2000) g(i)")
        q("select array_agg(i order by i % 100, i) from generate_series(1,2000) g(i)")
        q("select string_agg(i::text, ',' order by i desc) from generate_series(1,1000) g(i)")
        q("select sum(i order by i) from generate_series(1,2000) g(i)")
        q("select i % 10, array_agg(i order by i desc) from generate_series(1,1000) g(i) group by 1")
        q("select array_agg(distinct i % 50 order by i % 50) from generate_series(1,2000) g(i)")
    # 4. Knuth division hot path
    q("select sum(1/i::numeric) from generate_series(1,200) g(i)")
    q("select ln(i::numeric) from generate_series(1,200) g(i)")
    q("select sqrt(i::numeric) from generate_series(1,200) g(i)")

    s.sendall(b"X" + struct.pack("!i", 4))
    s.close()
    print("cg_v092: workload done")

if __name__ == "__main__":
    main()
