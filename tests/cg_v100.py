#!/usr/bin/env python3
"""cg_v100.py — callgrind/DHAT workload driver for the v1.00 partition paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises:
  1. Partition DDL: RANGE/LIST/HASH, PARTITION OF, ATTACH
  2. Partition routing: bulk inserts through the router
  3. BEFORE INSERT trigger firing during routing
  4. pg_class.relkind catalog scans
  5. Parent SELECT over many partitions
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

    # 1. Partition DDL
    q("CREATE TABLE cgp (a INT, b TEXT) PARTITION BY RANGE (a);")
    for i in range(8):
        lo, hi = i * 100 + 1, (i + 1) * 100 + 1
        q(f"CREATE TABLE cgp{i} PARTITION OF cgp FOR VALUES FROM ({lo}) TO ({hi});")
    q("CREATE TABLE cgl (a INT, b TEXT) PARTITION BY LIST (b);")
    q("CREATE TABLE cgl1 PARTITION OF cgl FOR VALUES IN ('a','b','c');")
    q("CREATE TABLE cgh (a INT) PARTITION BY HASH (a);")
    q("CREATE TABLE cgh0 PARTITION OF cgh FOR VALUES WITH (MODULUS 4, REMAINDER 0);")
    q("CREATE TABLE cgh1 PARTITION OF cgh FOR VALUES WITH (MODULUS 4, REMAINDER 1);")
    q("CREATE TABLE cgh2 PARTITION OF cgh FOR VALUES WITH (MODULUS 4, REMAINDER 2);")
    q("CREATE TABLE cgh3 PARTITION OF cgh FOR VALUES WITH (MODULUS 4, REMAINDER 3);")

    # 2. Bulk routing
    for i in range(N):
        q(f"INSERT INTO cgp SELECT g, 'x'||g FROM generate_series({i*50+1}, {(i+1)*50}) g;")
        q(f"INSERT INTO cgh SELECT generate_series({i*50+1}, {(i+1)*50});")

    # 3. Trigger firing during routing
    q("""CREATE FUNCTION cgtrigf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN new.b := new.b || '!'; RETURN new; END; $$""")
    q("CREATE TRIGGER cgtrig BEFORE INSERT ON cgp0 FOR EACH ROW EXECUTE FUNCTION cgtrigf()")
    for i in range(N):
        q(f"INSERT INTO cgp VALUES ({i+1}, 't{i}');")

    # 4. relkind catalog scans
    for i in range(N):
        q("SELECT relname, relkind FROM pg_class WHERE relkind = 'p';")
        q("SELECT relname, relkind FROM pg_class WHERE relkind = 'r';")

    # 5. Parent SELECT over many partitions
    for i in range(N):
        q("SELECT count(*) FROM cgp;")
        q("SELECT count(*) FROM cgh;")

    s.close()
    print(f"cg_v100 workload done (N={N})")

if __name__ == "__main__":
    main()
