#!/usr/bin/env python3
"""Valgrind workload for v0.48: exercises the new IS DISTINCT FROM and
CTAS paths (plus SELECT DISTINCT) through the wire protocol, so memcheck
covers the new expression evaluation, column-name inference, and
table-creation/insertion code.

Starts its own server on port 5560 with a fresh data dir, runs the
workload, shuts down. Run under:
  valgrind --tool=memcheck --error-exitcode=99 --leak-check=full \\
      --errors-for-leaks=yes python3 valgrind_workload_v48.py
"""
import socket, struct, subprocess, time, os

PORT = 5560
DATA_DIR = "/tmp/rgval48"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d


def connect():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        read_exact(s, ln - 4)
        if t == b"Z":
            break
    return s


def run_sql(s, sql):
    s.sendall(b"Q" + struct.pack("!i", len(sql.encode()) + 5) + sql.encode() + b"\x00")
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        read_exact(s, ln - 4)
        if t == b"Z":
            break


def main():
    client_only = os.environ.get("CLIENT_ONLY") == "1"
    proc = None
    if not client_only:
        subprocess.run(["rm", "-rf", DATA_DIR], check=False)
        os.makedirs(DATA_DIR, exist_ok=True)
        subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
        time.sleep(2)
        log = open("/tmp/rgval48.log", "w")
        proc = subprocess.Popen(
            [BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
            stdout=log, stderr=subprocess.STDOUT,
        )
        time.sleep(5)
    try:
        s = connect()
        stmts = [
            # IS DISTINCT FROM value paths: nulls, cross-type, NaN, text
            "CREATE TEMP TABLE vdist (f1 integer)",
            "INSERT INTO vdist VALUES (1), (2), (3), (NULL)",
            "SELECT f1, f1 IS DISTINCT FROM 2 FROM vdist",
            "SELECT f1, f1 IS DISTINCT FROM NULL FROM vdist",
            "SELECT f1, f1 IS DISTINCT FROM f1 FROM vdist",
            "SELECT f1, f1 IS NOT DISTINCT FROM f1 FROM vdist",
            "SELECT 1 IS DISTINCT FROM 1.0",
            "SELECT 'nan'::numeric IS DISTINCT FROM 'nan'::numeric",
            "SELECT 'nan'::float8 IS DISTINCT FROM 'nan'::float8",
            "SELECT 'abc' IS DISTINCT FROM 'abd'",
            "SELECT f1 FROM vdist WHERE f1 IS DISTINCT FROM 2",
            "SELECT f1 FROM vdist WHERE f1 IS NOT DISTINCT FROM NULL",
            # grouped (HAVING) path
            "CREATE TEMP TABLE vord (uid int, amt int)",
            "INSERT INTO vord VALUES (1, 10), (1, 20), (2, 5)",
            "SELECT uid, sum(amt) FROM vord GROUP BY uid HAVING sum(amt) IS DISTINCT FROM 30",
            "SELECT uid FROM vord GROUP BY uid HAVING count(*) IS NOT DISTINCT FROM 2",
            # SELECT DISTINCT paths
            "SELECT DISTINCT f1 FROM vdist",
            "SELECT DISTINCT f1 % 2, f1 FROM vdist",
            "SELECT DISTINCT f1 FROM vdist ORDER BY f1 DESC LIMIT 2",
            # CTAS headline paths
            "CREATE TABLE v_dg1 AS SELECT DISTINCT g%1000 FROM generate_series(0,9999) g",
            "CREATE TABLE v_dg2 AS SELECT DISTINCT (g%1000)::text FROM generate_series(0,9999) g",
            "CREATE TABLE v_dh1 AS SELECT DISTINCT g%1000 FROM generate_series(0,9999) g",
            "CREATE TABLE v_dh2 AS SELECT DISTINCT (g%1000)::text FROM generate_series(0,9999) g",
            "SELECT count(*) FROM v_dg1",
            "SELECT count(*) FROM v_dh2",
            "CREATE TABLE v_alias (x, y) AS SELECT 1+1, 2+2",
            "CREATE TEMP TABLE v_tmp AS SELECT 42 AS a",
            "CREATE TABLE v_nodata AS SELECT 1 AS a WITH NO DATA",
            "CREATE TABLE IF NOT EXISTS v_nodata AS SELECT 1 AS a",
            "DROP TABLE v_dg1",
            "DROP TABLE v_dg2",
            "DROP TABLE v_dh1",
            "DROP TABLE v_dh2",
        ]
        for q in stmts:
            run_sql(s, q)
        # prepared-statement (extended protocol) path with IS DISTINCT FROM
        # Parse/Bind/Describe/Execute for a $1 IS DISTINCT FROM $2 query.
        def msg(t, body):
            s.sendall(t + struct.pack("!i", len(body) + 4) + body)

        msg(b"P", b"\x00SELECT $1 IS DISTINCT FROM $2\x00\x00\x00")
        msg(b"B", b"\x00\x00\x00\x00\x00\x02" + struct.pack("!i", 4) + b"0001" + struct.pack("!i", 4) + b"0002" + b"\x00\x00\x00\x00")
        msg(b"D", b"S\x00")
        msg(b"E", b"\x00\x00\x00\x00\x00")
        msg(b"S", b"")
        while True:
            t = read_exact(s, 1)
            ln = struct.unpack("!i", read_exact(s, 4))[0]
            read_exact(s, ln - 4)
            if t == b"Z":
                break
        s.close()
        print(f"valgrind workload v48: {len(stmts) + 1} statements done")
    finally:
        if proc is not None:
            proc.terminate()


if __name__ == "__main__":
    main()
