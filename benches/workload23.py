#!/usr/bin/env python3
"""v0.23 join-heavy workload benchmark.

Measures throughput of the merged USING/NATURAL join paths added in
v0.23: merged-key layout planning, null-extended outer joins,
qualified-star expansion in source order, USING aliases, whole-join
aliases, and table column alias lists.

Needs a running server on 127.0.0.1:5433 (it creates its own wj1/wj2
tables, dropping any leftovers first).
"""
import socket
import struct
import sys
import time

HOST = "127.0.0.1"
PORT = 5433

SETUP = [
    "DROP TABLE IF EXISTS wj1",
    "DROP TABLE IF EXISTS wj2",
    "CREATE TABLE wj1(i int, j int, t text)",
    "CREATE TABLE wj2(i int, k int)",
]

QUERIES = [
    # merged USING inner join, full star (exercises row_plan)
    "SELECT * FROM wj1 JOIN wj2 USING (i)",
    # merged key + non-key projection
    "SELECT i, j, k FROM wj1 JOIN wj2 USING (i)",
    # qualified star expansion in source order (src_ord sort)
    "SELECT wj1.*, wj2.* FROM wj1 JOIN wj2 USING (i)",
    # outer joins with null extension + coalesced keys
    "SELECT * FROM wj1 LEFT JOIN wj2 USING (i)",
    "SELECT * FROM wj1 RIGHT JOIN wj2 USING (i)",
    "SELECT * FROM wj1 FULL JOIN wj2 USING (i)",
    # USING alias: x.* exposes only merged keys
    "SELECT x.* FROM wj1 JOIN wj2 USING (i) AS x",
    # whole-join alias with hidden columns dropped
    "SELECT * FROM (wj1 JOIN wj2 USING (i)) AS x",
    # NATURAL JOIN merge
    "SELECT * FROM wj1 NATURAL JOIN wj2",
    # table column alias lists (base + derived)
    "SELECT a, b FROM wj1 AS t(a, b, c)",
    "SELECT x FROM (SELECT i, j FROM wj1) AS s(x, y)",
    # unqualified merged key in WHERE / ORDER BY
    "SELECT i FROM wj1 JOIN wj2 USING (i) WHERE i > 250 ORDER BY i",
]


def read_msg(sock):
    hdr = b""
    while len(hdr) < 5:
        chunk = sock.recv(5 - len(hdr))
        if not chunk:
            return None, None
        hdr += chunk
    typ = hdr[0:1]
    ln = struct.unpack("!I", hdr[1:5])[0]
    body = b""
    while len(body) < ln - 4:
        chunk = sock.recv(ln - 4 - len(body))
        if not chunk:
            break
        body += chunk
    return typ, body


def startup(sock):
    params = b"user\x00postgres\x00database\x00postgres\x00\x00"
    sock.sendall(struct.pack("!I", len(params) + 8) + struct.pack("!I", 196608) + params)
    while True:
        typ, _ = read_msg(sock)
        if typ == b"Z":
            break


def simple_query(sock, q):
    msg = b"Q" + struct.pack("!I", len(q) + 5) + q.encode() + b"\x00"
    sock.sendall(msg)
    rows = 0
    while True:
        typ, body = read_msg(sock)
        if typ is None:
            raise RuntimeError("connection closed")
        if typ == b"D":
            rows += 1
        elif typ == b"C":
            pass
        elif typ == b"Z":
            break
        elif typ == b"E":
            raise RuntimeError(f"query error: {body}")
    return rows


def main():
    sock = socket.create_connection((HOST, PORT), timeout=30)
    try:
        startup(sock)
        for q in SETUP:
            try:
                simple_query(sock, q)
            except RuntimeError as e:
                # DROP IF EXISTS on a missing table is fine; anything
                # else is real.
                if "does not exist" not in str(e):
                    raise
        # 500-row tables: 250k nested-loop pairs per join, 1:1 matches.
        vals1 = ",".join(f"({i},{i * 10},'t{i}')" for i in range(1, 501))
        simple_query(sock, f"INSERT INTO wj1 VALUES {vals1}")
        vals2 = ",".join(f"({i},{i * 100})" for i in range(1, 501))
        simple_query(sock, f"INSERT INTO wj2 VALUES {vals2}")
        # Warmup
        for _ in range(3):
            for q in QUERIES:
                simple_query(sock, q)
        # Timed run
        n_iter = 10
        start = time.perf_counter()
        total = 0
        for _ in range(n_iter):
            for q in QUERIES:
                simple_query(sock, q)
                total += 1
        elapsed = time.perf_counter() - start
        qps = total / elapsed if elapsed > 0 else 0
        print(f"{total} queries in {elapsed:.2f}s, {qps:.1f} qps")
    finally:
        sock.close()


if __name__ == "__main__":
    main()
