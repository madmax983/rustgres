#!/usr/bin/env python3
"""cg_v089.py — callgrind/DHAT driver for the v0.89 transaction/cursor paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises:
  1. COMMIT/ROLLBACK AND CHAIN (inside and outside txn)
  2. Read-only transactions with temp-table DML
  3. Cursor DECLARE / FETCH / MOVE with savepoints and ROLLBACK TO
  4. Lazy cursor DECLARE + FETCH (error path)
  5. STABLE vs VOLATILE SQL functions in UPDATE
  6. Bare FETCH/MOVE syntax
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

    # setup
    q("CREATE TABLE cg89(a int, b int)")
    q("INSERT INTO cg89 SELECT g, 0 FROM generate_series(1, 1000) g")
    q("CREATE FUNCTION cg89_s() RETURNS int STABLE AS $$ SELECT 787 $$ LANGUAGE sql")
    q("CREATE FUNCTION cg89_v() RETURNS int VOLATILE AS $$ SELECT 787 $$ LANGUAGE sql")

    # 1. AND CHAIN
    for _ in range(20):
        q("COMMIT AND CHAIN")
        q("ROLLBACK AND CHAIN")
        q("BEGIN")
        q("INSERT INTO cg89 VALUES (1, 1)")
        q("COMMIT AND CHAIN")
        q("INSERT INTO cg89 VALUES (2, 2)")
        q("ROLLBACK")

    # 2. Read-only with temp DML
    for _ in range(20):
        q("BEGIN READ ONLY")
        q("CREATE TEMP TABLE cg89t(x int)")
        q("INSERT INTO cg89t VALUES (1)")
        q("UPDATE cg89t SET x = 2")
        q("DELETE FROM cg89t")
        q("ROLLBACK")

    # 3. Cursors + savepoints
    for _ in range(20):
        q("BEGIN")
        q("DECLARE cg89c CURSOR FOR SELECT a FROM cg89 ORDER BY a")
        q("FETCH 100 FROM cg89c")
        q("SAVEPOINT s1")
        q("FETCH 100 FROM cg89c")
        q("ROLLBACK TO SAVEPOINT s1")
        q("FETCH 100 FROM cg89c")
        q("CLOSE cg89c")
        q("COMMIT")

    # 4. Lazy cursor error path
    for _ in range(20):
        q("BEGIN")
        q("DECLARE cg89l CURSOR FOR SELECT a/0 FROM cg89")
        q("SAVEPOINT s2")
        q("FETCH 10 FROM cg89l")
        q("ROLLBACK TO SAVEPOINT s2")
        q("FETCH 10 FROM cg89l")
        q("COMMIT")

    # 5. STABLE vs VOLATILE in UPDATE
    for _ in range(10):
        q("UPDATE cg89 SET b = cg89_s()")
        q("UPDATE cg89 SET b = cg89_v()")

    # 6. Bare FETCH/MOVE
    for _ in range(20):
        q("BEGIN")
        q("DECLARE cg89b CURSOR FOR SELECT a FROM cg89 ORDER BY a")
        q("FETCH cg89b")
        q("MOVE cg89b")
        q("FETCH cg89b")
        q("COMMIT")

    print("cg_v089 workload done")


if __name__ == "__main__":
    main()
