#!/usr/bin/env python3
"""protocol_test80.py — v0.78 protocol coverage.

v0.78 changes under test:
A. EXPLAIN resolves CTE names (join, chaining, shadowing a real table):
   previously EXPLAIN of a query referencing a CTE failed with 42P01,
   which poisoned explicit transactions.
B. array(SELECT ...) reports proper array OIDs in RowDescription:
   int4[] -> 1007, text[] -> 1009, with PG array-literal wire text.
C. GROUP BY grouping-set syntax ((), ROLLUP, CUBE, GROUPING SETS,
   DISTINCT): parses and executes; bare columns not in the current
   set come back NULL; EXPLAIN of grouping-set queries no longer
   poisons an explicit transaction.

This test manages its own server on port 5552.
"""
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5552
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = 0
failed = 0


def check(name, cond, extra=""):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name} {extra}")


def cstr(s):
    return s.encode() + b"\x00"


def read_msg(s):
    typ = s.recv(1)
    ln = struct.unpack("!i", s.recv(4))[0]
    payload = b""
    while len(payload) < ln - 4:
        chunk = s.recv(ln - 4 - len(payload))
        if not chunk:
            raise RuntimeError("connection closed")
        payload += chunk
    return typ, payload


def parse_fields(payload):
    out = {}
    pos = 0
    while pos < len(payload) - 1:
        kind = payload[pos:pos + 1]
        end = payload.index(b"\x00", pos + 1)
        out[kind] = payload[pos + 1:end].decode(errors="replace")
        pos = end + 1
    return out


def err_code(msgs):
    for t, p in msgs:
        if t == b"E":
            return parse_fields(p).get(b"C")
    return None


def rowdesc_oids(msgs):
    """Type OIDs from the first RowDescription, or None."""
    for t, p in msgs:
        if t == b"T":
            oids = []
            n = struct.unpack("!h", p[:2])[0]
            pos = 2
            for _ in range(n):
                end = p.index(b"\x00", pos)
                pos = end + 1
                pos += 4 + 2  # table oid + column attr
                (oid,) = struct.unpack("!i", p[pos:pos + 4])
                oids.append(oid)
                pos += 4 + 2 + 4 + 2  # size + modifier + format
            return oids
    return None


def parse_datarow(payload):
    out = []
    nfields = struct.unpack("!h", payload[:2])[0]
    pos = 2
    for _ in range(nfields):
        ln = struct.unpack("!i", payload[pos:pos + 4])[0]
        pos += 4
        if ln == -1:
            out.append(None)
        else:
            out.append(payload[pos:pos + ln].decode())
            pos += ln
    return out


def rows_of(msgs):
    return [parse_datarow(p) for t, p in msgs if t == b"D"]


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = read_msg(self.s)
            if t == b"Z":
                break

    def query(self, sql):
        self.s.sendall(b"Q" + struct.pack("!i", len(sql.encode()) + 5)
                       + sql.encode() + b"\x00")
        msgs = []
        while True:
            t, p = read_msg(self.s)
            msgs.append((t, p))
            if t == b"Z":
                return msgs

    def close(self):
        self.s.close()


def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", data_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.terminate()
    raise RuntimeError("server did not start")


def stop_server(proc):
    proc.terminate()
    proc.wait()


def main():
    data_dir = tempfile.mkdtemp(prefix="rg80_")
    proc = start_server(data_dir)
    try:
        c = Conn()
        c.query("CREATE TABLE pt80 (a int, b int)")
        c.query("INSERT INTO pt80 VALUES (1, 10), (1, 20), (2, 30)")

        # ---- A. EXPLAIN resolves CTEs ----
        m = c.query("EXPLAIN (COSTS OFF) WITH v AS (SELECT 'x' AS id) "
                    "SELECT count(*) FROM pt80 JOIN v ON true")
        check("A1 explain CTE join: no error", err_code(m) is None, err_code(m))
        plan = " ".join(r[0] for r in rows_of(m))
        check("A2 plan scans CTE v", "v" in plan, plan[:80])

        m = c.query("EXPLAIN (COSTS OFF) WITH a AS (SELECT 1 AS x), "
                    "b AS (SELECT x FROM a) SELECT * FROM b")
        check("A3 explain chained CTEs: no error", err_code(m) is None, err_code(m))

        m = c.query("EXPLAIN (COSTS OFF) WITH pt80 AS (SELECT 1 AS a) "
                    "SELECT * FROM pt80")
        check("A4 explain CTE shadowing table: no error", err_code(m) is None,
              err_code(m))

        # ---- B. array(SELECT ...) RowDescription OIDs ----
        m = c.query("SELECT array(SELECT generate_series(1, 3))")
        check("B1 int array OID 1007", rowdesc_oids(m) == [1007],
              rowdesc_oids(m))
        check("B2 int array wire text", rows_of(m) == [["{1,2,3}"]],
              rows_of(m))

        m = c.query("SELECT array(SELECT 'a' UNION ALL SELECT 'b')")
        check("B3 text array OID 1009", rowdesc_oids(m) == [1009],
              rowdesc_oids(m))
        check("B4 text array wire text", rows_of(m) == [["{a,b}"]],
              rows_of(m))

        # ---- C. grouping sets ----
        m = c.query("SELECT a, b, count(*) FROM pt80 "
                    "GROUP BY GROUPING SETS ((a), (b), ()) ORDER BY 1, 2, 3")
        check("C1 grouping sets: no error", err_code(m) is None, err_code(m))
        check("C2 grouping sets rows", rows_of(m) == [
            ["1", None, "2"], ["2", None, "1"],
            [None, "10", "1"], [None, "20", "1"], [None, "30", "1"],
            [None, None, "3"],
        ], rows_of(m))

        m = c.query("SELECT a, count(*) FROM pt80 GROUP BY ROLLUP (a) "
                    "ORDER BY 1, 2")
        check("C3 rollup rows", rows_of(m) == [["1", "2"], ["2", "1"],
                                               [None, "3"]], rows_of(m))

        m = c.query("SELECT count(*) FROM pt80 GROUP BY ()")
        check("C4 empty grouping set", rows_of(m) == [["3"]], rows_of(m))

        # EXPLAIN of grouping-set syntax inside an explicit transaction
        # must not poison it (the v0.78 join.sql regression).
        c.query("BEGIN")
        m = c.query("EXPLAIN SELECT a, count(*) FROM pt80 "
                    "GROUP BY GROUPING SETS ((), (a))")
        check("C5 explain grouping sets: no error", err_code(m) is None,
              err_code(m))
        m = c.query("SELECT count(*) FROM pt80")
        check("C6 txn usable after explain", err_code(m) is None and
              rows_of(m) == [["3"]], (err_code(m), rows_of(m)))
        c.query("COMMIT")

        c.close()
    finally:
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol_test80: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
