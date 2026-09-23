#!/usr/bin/env python3
"""protocol_test78.py — v0.76 compatibility cluster.

Covers the v0.76 feature set, grounded in the pg_regress join/select/
subselect corpus:

A. `UPDATE ... FROM` — the FROM clause joins extra relations into the
   update (PG19 DML). SET/WHERE/RETURNING can reference FROM columns.
   `UPDATE u78a SET code = code + delta FROM u78b
    WHERE u78a.id = u78b.id` updates matching rows; RETURNING can
   project `u78b.delta`.

B. `pg_typeof(x)` — returns the PG type name of its argument as text:
   `pg_typeof(1)` -> 'integer', `pg_typeof('foo')` -> 'text',
   `pg_typeof(1.5)` -> 'numeric', `pg_typeof(NULL)` -> 'unknown'.

C. `json_array(...)` — builds a JSON array value:
   `json_array(1, 2, 3)` -> '[1, 2, 3]',
   `json_array('a', 'b')` -> '["a", "b"]'.

D. `current_timestamp(precision)` accepts the optional precision argument
   (PG allows 0-6); `now(n)` / `transaction_timestamp(n)` are 42883 in
   PG19 (plain 0-argument functions, unlike the special CURRENT_TIMESTAMP
   syntax) and must error;
   `EXTRACT(DOW FROM current_timestamp(0))` works.

E. `sum()` widens like PG19: `sum(int)`/`sum(smallint)` return bigint,
   `sum(bigint)` returns numeric. `SELECT sum(x)` over
   (2147483647, 1) yields 2147483648 instead of erroring.

F. Quantified comparisons against a VALUES list:
   `i <> ALL (VALUES (2), (3))` keeps rows 1 and 4;
   `i = ANY (VALUES (1), (3))` keeps row 1.

G. `WITH ... AS NOT MATERIALIZED` inside a parenthesized set-operation
   branch (the inner-WITH form from the recursive-CTE regression test)
   parses and runs.

H. `ALTER TABLE ... ADD CONSTRAINT name NOT NULL col NOT VALID` is
   accepted (NOT VALID defers validation of existing rows).

This test manages its own server on port 5550 (so it never collides
with the conformance runner) and is RED without the v0.76 support.
"""

import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5550
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
    data_dir = tempfile.mkdtemp(prefix="rg78_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # A. UPDATE ... FROM.
        r = c.do_sql("CREATE TABLE u78a(id int, code int)")
        check("A0 setup a", r["err"] is None)
        r = c.do_sql("CREATE TABLE u78b(id int, delta int)")
        check("A0 setup b", r["err"] is None)
        r = c.do_sql("INSERT INTO u78a VALUES (1, 10), (2, 20), (3, 30)")
        check("A0 insert a", r["err"] is None)
        r = c.do_sql("INSERT INTO u78b VALUES (1, 1), (2, 2)")
        check("A0 insert b", r["err"] is None)
        r = c.do_sql("UPDATE u78a SET code = code + delta FROM u78b "
                     "WHERE u78a.id = u78b.id")
        check("A1 update from", r["err"] is None and r["tag"] == "UPDATE 2")
        r = c.do_sql("SELECT id, code FROM u78a ORDER BY id")
        check("A2 update from values",
              r["err"] is None and r["rows"] == [["1", "11"],
                                                 ["2", "22"],
                                                 ["3", "30"]])
        r = c.do_sql("UPDATE u78a SET code = code + delta FROM u78b "
                     "WHERE u78a.id = u78b.id RETURNING u78a.id, u78b.delta")
        check("A3 update from returning",
              r["err"] is None and sorted(r["rows"]) == [["1", "1"],
                                                        ["2", "2"]])

        # B. pg_typeof.
        r = c.do_sql("SELECT pg_typeof(1)")
        check("B1 typeof int",
              r["err"] is None and r["rows"] == [["integer"]])
        r = c.do_sql("SELECT pg_typeof('foo')")
        check("B2 typeof text",
              r["err"] is None and r["rows"] == [["text"]])
        r = c.do_sql("SELECT pg_typeof(1.5)")
        check("B3 typeof numeric",
              r["err"] is None and r["rows"] == [["numeric"]])
        r = c.do_sql("SELECT pg_typeof(NULL)")
        check("B4 typeof null",
              r["err"] is None and r["rows"] == [["unknown"]])
        r = c.do_sql("SELECT pg_typeof(true)")
        check("B5 typeof bool",
              r["err"] is None and r["rows"] == [["boolean"]])

        # C. json_array.
        r = c.do_sql("SELECT json_array(1, 2, 3)")
        check("C1 json_array ints",
              r["err"] is None and r["rows"] == [["[1, 2, 3]"]])
        r = c.do_sql("SELECT json_array('a', 'b')")
        check("C2 json_array texts",
              r["err"] is None and r["rows"] == [['["a", "b"]']])
        r = c.do_sql("SELECT json_array()")
        check("C3 json_array empty",
              r["err"] is None and r["rows"] == [["[]"]])

        # D. current_timestamp(precision).
        r = c.do_sql("SELECT current_timestamp(0)")
        check("D1 current_timestamp(0)", r["err"] is None)
        r = c.do_sql("SELECT EXTRACT(DOW FROM current_timestamp(0))")
        check("D2 extract dow", r["err"] is None and len(r["rows"]) == 1)
        r = c.do_sql("SELECT now(3)")
        check("D3 now(3) is 42883 like PG19", r["err"] is not None and "42883" in r["err"])
        r = c.do_sql("SELECT transaction_timestamp(6)")
        check("D4 transaction_timestamp(6) is 42883 like PG19",
              r["err"] is not None and "42883" in r["err"])

        # E. sum() widening.
        r = c.do_sql("CREATE TABLE s78(x int)")
        check("E0 setup", r["err"] is None)
        r = c.do_sql("INSERT INTO s78 VALUES (2147483647), (1)")
        check("E0 insert", r["err"] is None)
        r = c.do_sql("SELECT sum(x) FROM s78")
        check("E1 sum int widens",
              r["err"] is None and r["rows"] == [["2147483648"]])
        r = c.do_sql("SELECT pg_typeof(sum(x)) FROM s78")
        check("E2 sum int type",
              r["err"] is None and r["rows"] == [["bigint"]])

        # F. Quantified comparison vs VALUES.
        r = c.do_sql("SELECT i FROM (VALUES (1), (2), (3), (4)) v(i) "
                     "WHERE i <> ALL (VALUES (2), (3)) ORDER BY i")
        check("F1 neq all values",
              r["err"] is None and r["rows"] == [["1"], ["4"]])
        r = c.do_sql("SELECT i FROM (VALUES (1), (2)) v(i) "
                     "WHERE i = ANY (VALUES (1), (3)) ORDER BY i")
        check("F2 eq any values",
              r["err"] is None and r["rows"] == [["1"]])
        r = c.do_sql("SELECT i FROM (VALUES (1), (2), (3)) v(i) "
                     "WHERE i > SOME (VALUES (1), (2)) ORDER BY i")
        check("F3 gt some values",
              r["err"] is None and r["rows"] == [["2"], ["3"]])

        # G. Inner WITH ... AS NOT MATERIALIZED in a set-op branch.
        r = c.do_sql(
            "WITH x(a) AS (VALUES ('a'), ('b')) "
            "SELECT * FROM "
            "(WITH z AS NOT MATERIALIZED (SELECT * FROM x) SELECT * FROM z) q "
            "ORDER BY a")
        check("G1 inner with not materialized",
              r["err"] is None and r["rows"] == [["a"], ["b"]])

        # H. ADD CONSTRAINT ... NOT NULL ... NOT VALID.
        r = c.do_sql("CREATE TABLE nn78(id int)")
        check("H0 setup", r["err"] is None)
        r = c.do_sql("INSERT INTO nn78 VALUES (NULL)")
        check("H0 insert null", r["err"] is None)
        r = c.do_sql("ALTER TABLE nn78 ADD CONSTRAINT nn78_nn "
                     "NOT NULL id NOT VALID")
        check("H1 add not null not valid", r["err"] is None)

        c.close()
    finally:
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol 78: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
