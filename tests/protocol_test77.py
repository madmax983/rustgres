#!/usr/bin/env python3
"""protocol_test77.py — v0.75 compatibility cluster.

Covers the v0.75 feature set, grounded in the pg_regress join/select/
subselect corpus:

A. Parenthesized set-operation scalar subqueries:
   `SELECT ((SELECT 2) UNION SELECT 2)` and
   `SELECT (((SELECT 2)) UNION SELECT 2)` — both return 2.
   Ordinary `(SELECT 2)` and `((1+2))` still work.

B. `CREATE TEMP[ORARY] SEQUENCE` — accepted; `nextval()` works.

C. `WITH x AS NOT MATERIALIZED (...)` / `AS MATERIALIZED (...)` —
   the hint is accepted and ignored.

D. Schema-qualified casts: `1::information_schema.sql_identifier`
   works (treated as varchar).

E. Parenthesized join in FROM:
   `((select ...) s LEFT JOIN (select ...) t USING (x))` — the
   parenthesized derived table can be a join operand.

F. `SELECT EXISTS(...)` names the output column `exists` (not `?column?`).

G. `CREATE STATISTICS ...` — validated no-op.

H. `VACUUM (ANALYZE) table` — parenthesized options accepted.

I. FK referencing a standalone unique index (not just a constraint).

This test manages its own server on port 5549 (so it never collides
with the conformance runner) and is RED without the v0.75 support.
"""

import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5549
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = 0
failed = 0


def check(name, cond):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")


def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError("server did not start")


def stop_server(proc):
    try:
        proc.terminate()
        proc.wait(timeout=10)
    except Exception:
        proc.kill()
    time.sleep(1.0)


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            typ, _ = self._read_msg()
            if typ == b"Z":
                break

    def _read_exact(self, n):
        d = b""
        while len(d) < n:
            c = self.s.recv(n - len(d))
            if not c:
                raise RuntimeError("connection closed")
            d += c
        return d

    def _read_msg(self):
        typ = self._read_exact(1)
        ln = struct.unpack("!i", self._read_exact(4))[0]
        return typ, self._read_exact(ln - 4)

    def do_sql(self, q):
        qb = q.encode()
        self.s.sendall(struct.pack("!c", b"Q")
                       + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
        tag = None
        err = None
        rows = []
        hdr = []
        nfields = 0
        while True:
            typ, payload = self._read_msg()
            if typ == b"T":
                nfields = struct.unpack("!H", payload[:2])[0]
                pos = 2
                for _ in range(nfields):
                    end = payload.index(b"\x00", pos)
                    hdr.append(payload[pos:end].decode())
                    pos = end + 1 + 18
            elif typ == b"D":
                pos = 2
                row = []
                for _ in range(nfields):
                    ln = struct.unpack("!i", payload[pos:pos+4])[0]
                    pos += 4
                    if ln == -1:
                        row.append(None)
                    else:
                        row.append(payload[pos:pos+ln].decode())
                        pos += ln
                rows.append(row)
            elif typ == b"C":
                tag = payload[:-1].decode()
            elif typ == b"E":
                err = payload.decode(errors="replace")
            elif typ == b"Z":
                break
        return {"tag": tag, "err": err, "rows": rows, "hdr": hdr}

    def close(self):
        self.s.close()


def main():
    data_dir = tempfile.mkdtemp(prefix="rg77_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # A. Parenthesized setop scalar subqueries.
        r = c.do_sql("SELECT ((SELECT 2) UNION SELECT 2)")
        check("A1 paren setop scalar", r["err"] is None and r["rows"] == [["2"]])
        r = c.do_sql("SELECT (((SELECT 2)) UNION SELECT 2)")
        check("A2 triple paren setop scalar", r["err"] is None and r["rows"] == [["2"]])
        r = c.do_sql("SELECT (SELECT 2)")
        check("A3 plain scalar subquery", r["err"] is None and r["rows"] == [["2"]])
        r = c.do_sql("SELECT ((1+2))")
        check("A4 paren expr", r["err"] is None and r["rows"] == [["3"]])

        # B. CREATE TEMP SEQUENCE.
        r = c.do_sql("CREATE TEMP SEQUENCE ts77")
        check("B1 create temp sequence", r["err"] is None)
        r = c.do_sql("SELECT nextval('ts77')")
        check("B2 nextval 1", r["err"] is None and r["rows"] == [["1"]])
        r = c.do_sql("SELECT nextval('ts77')")
        check("B3 nextval 2", r["err"] is None and r["rows"] == [["2"]])
        r = c.do_sql("CREATE TEMPORARY SEQUENCE ts77b")
        check("B4 create temporary sequence", r["err"] is None)

        # C. NOT MATERIALIZED / MATERIALIZED hints.
        r = c.do_sql("WITH t AS NOT MATERIALIZED (SELECT 1 AS x) SELECT * FROM t")
        check("C1 not materialized", r["err"] is None and r["rows"] == [["1"]])
        r = c.do_sql("WITH t AS MATERIALIZED (SELECT 2 AS x) SELECT * FROM t")
        check("C2 materialized", r["err"] is None and r["rows"] == [["2"]])

        # D. Schema-qualified cast.
        r = c.do_sql("SELECT 1::information_schema.sql_identifier")
        check("D1 sql_identifier cast", r["err"] is None and r["rows"] == [["1"]])
        r = c.do_sql("SELECT 1::pg_catalog.int4")
        check("D2 pg_catalog.int4 cast", r["err"] is None and r["rows"] == [["1"]])

        # E. Parenthesized join in FROM.
        r = c.do_sql("CREATE TABLE j77a(f1 int)")
        check("E0 setup a", r["err"] is None)
        r = c.do_sql("CREATE TABLE j77b(q1 int)")
        check("E0 setup b", r["err"] is None)
        r = c.do_sql("INSERT INTO j77a VALUES (1)")
        check("E0 insert a", r["err"] is None)
        r = c.do_sql("INSERT INTO j77b VALUES (2)")
        check("E0 insert b", r["err"] is None)
        r = c.do_sql("SELECT s.f1 FROM ((SELECT f1 FROM j77a) s LEFT JOIN (SELECT q1 FROM j77b) t ON true)")
        check("E1 paren join", r["err"] is None and r["rows"] == [["1"]])
        r = c.do_sql("SELECT * FROM ((SELECT 1 AS x) ss)")
        check("E2 redundant paren alias", r["err"] is None and r["rows"] == [["1"]])

        # F. EXISTS column name.
        r = c.do_sql("SELECT EXISTS(SELECT 1)")
        check("F1 exists colname", r["err"] is None and r["hdr"] == ["exists"])

        # G. CREATE STATISTICS no-op.
        r = c.do_sql("CREATE STATISTICS st77 (ndistinct) ON a, b FROM j77a")
        check("G1 create statistics", r["err"] is None)

        # H. VACUUM (ANALYZE).
        r = c.do_sql("VACUUM (ANALYZE) j77a")
        check("H1 vacuum analyze paren", r["err"] is None)

        # I. FK referencing unique index.
        r = c.do_sql("CREATE TABLE pk77(x int, y int)")
        check("I0 setup pk", r["err"] is None)
        r = c.do_sql("CREATE UNIQUE INDEX pk77_idx ON pk77(x, y)")
        check("I1 unique index", r["err"] is None)
        r = c.do_sql("CREATE TABLE fk77(a int, b int, FOREIGN KEY (a, b) REFERENCES pk77(x, y))")
        check("I2 fk via unique index", r["err"] is None)

        c.close()
    finally:
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol 77: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
