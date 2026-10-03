#!/usr/bin/env python3
"""cg_v088.py — callgrind/DHAT driver for the v0.88 index-DDL hot paths.

Assumes a rustgres server already running (under callgrind or DHAT).
Exercises:
  1. CREATE INDEX with DESC / NULLS FIRST|LAST (parse + exec + storage)
  2. Expression + partial index creation (catalog-only path)
  3. ORDER BY queries against tables with directed indexes (planner
     fast-path eligibility check -> explicit sort fallback)
  4. DML with catalog-only indexes present (insert/update/delete)
  5. pg_attribute virtual-table scans
  6. Multi-name DROP INDEX
"""
import socket, struct

PORT = 5434


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
    params = b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)

    def rd(n):
        d = b""
        while len(d) < n:
            c = s.recv(n - len(d))
            if not c:
                raise RuntimeError("closed")
            d += c
        return d

    while True:
        t = rd(1)
        ln = struct.unpack("!i", rd(4))[0]
        rd(ln - 4)
        if t == b"Z":
            break

    def simple(sql):
        s.sendall(b"Q" + struct.pack("!i", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        while True:
            t = rd(1)
            ln = struct.unpack("!i", rd(4))[0]
            rd(ln - 4)
            if t == b"Z":
                break

    simple("CREATE TABLE cg88 (a int, b int, c text)")
    simple("INSERT INTO cg88 SELECT i, i % 97, 'v' || i FROM generate_series(1,2000) i")

    # 1. Directed-index DDL (parse + exec + storage metadata).
    for i in range(30):
        simple(f"CREATE INDEX cg88_d{i} ON cg88 (a DESC)")
        simple(f"CREATE INDEX cg88_n{i} ON cg88 (b DESC NULLS FIRST)")
    # 2. Catalog-only DDL.
    for i in range(30):
        simple(f"CREATE UNIQUE INDEX cg88_e{i} ON cg88 ((a * {i + 1}))")
        simple(f"CREATE INDEX cg88_p{i} ON cg88 (b) WHERE b > {i}")
    # 3. ORDER BY against directed indexes (planner check + sort).
    for i in range(100):
        simple("SELECT a FROM cg88 ORDER BY a")
        simple("SELECT a FROM cg88 ORDER BY a DESC")
        simple("SELECT b FROM cg88 ORDER BY b DESC NULLS FIRST")
    # 4. DML with catalog-only indexes present.
    for i in range(50):
        simple(f"INSERT INTO cg88 VALUES ({3000 + i}, {i}, 'w{i}')")
        simple(f"UPDATE cg88 SET b = {i} WHERE a = {3000 + i}")
        simple(f"DELETE FROM cg88 WHERE a = {3000 + i}")
    # 5. pg_attribute scans.
    for i in range(50):
        simple("SELECT attname, attnum FROM pg_attribute WHERE attrelid = "
               "(SELECT oid FROM pg_class WHERE relname = 'cg88')")
    # 6. Multi-name drops.
    drops = ", ".join(f"cg88_d{i}" for i in range(30))
    simple(f"DROP INDEX {drops}")
    drops = ", ".join(f"cg88_n{i}" for i in range(30))
    simple(f"DROP INDEX {drops}")
    drops = ", ".join(f"cg88_e{i}" for i in range(30))
    simple(f"DROP INDEX {drops}")
    drops = ", ".join(f"cg88_p{i}" for i in range(30))
    simple(f"DROP INDEX {drops}")

    s.sendall(b"X" + struct.pack("!i", 4))
    s.close()
    print("cg_v088 workload done")


if __name__ == "__main__":
    main()
