#!/usr/bin/env python3
"""protocol_test75.py — v0.73 whole-row references / whole-row Vars.

Covers PostgreSQL 19 whole-row reference semantics, grounded in the
REL_19_STABLE regression outputs (subselect.out, join.out, insert.out):

1. Bare relation reference: `select view_a from view_a` -> `(42)`.
2. Correlated whole-row in scalar subqueries, incl. doubly nested:
   `select (select view_a) from view_a` -> `(42)`.
3. Whole-row in expression position: `(a.*)::text` -> `(42)`.
4. Derived-table whole-row has no junk columns:
   `select q from (select max(f1) ... group by f1 ...) q` -> 1-col rows.
5. USING-alias whole-row shape: `x.*` over a join alias merges to the
   join's output columns; `row_to_json(x.*)` -> `{"i":1}`.
6. `INSERT ... RETURNING tbl` yields the composite, in the *parent's*
   column order for partitioned tables (PG19 insert.out).
7. Aggregate / IS NULL null semantics: `count(t2.*)` skips the
   null-extended outer-join row; `(t2.*) IS NULL` is true for it.
8. Genuine errors are preserved: unknown column still 42703, unknown
   qualifier still fails, column references still win over range names.

This test manages its own server on port 5547 (so it never collides
with the conformance runner) and is RED without the v0.73 whole-row
support.
"""

import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5547
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
                    ln = struct.unpack("!i", payload[pos:pos + 4])[0]
                    pos += 4
                    if ln < 0:
                        row.append(None)
                    else:
                        row.append(payload[pos:pos + ln].decode())
                        pos += ln
                rows.append(tuple(row))
            elif typ == b"C":
                tag = payload[:-1].decode()
            elif typ == b"E":
                i = 0
                code = None
                while i < len(payload) - 1:
                    f = payload[i:i + 1]
                    end = payload.find(b"\x00", i + 1)
                    if f == b"C":
                        code = payload[i + 1:end].decode()
                    i = end + 1
                err = code
            elif typ == b"Z":
                break
        return err, tag, hdr, rows

    def close(self):
        self.s.close()


def main():
    data_dir = tempfile.mkdtemp(prefix="rg75_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        for stmt in [
            "create table p75_t(a int, b text)",
            "insert into p75_t values (1,'foo'),(2,null)",
            "create table p75_table_a(id integer)",
            "insert into p75_table_a values (42)",
            "create view p75_view_a as select * from p75_table_a",
            "create table p75_j1(i integer)",
            "create table p75_j2(i integer)",
            "insert into p75_j1 values (1)",
            "insert into p75_j2 values (1)",
            "create table p75_t1(a int)",
            "create table p75_t2(a int)",
            "insert into p75_t1 values (1),(2)",
            "insert into p75_t2 values (2),(3)",
            "create table p75_int4(f1 int)",
            "insert into p75_int4 values (-2147483647),(-123456),(0),(123456),(2147483647)",
            "create table p75_parted (a int) partition by list (a)",
            "create table p75_parted1 partition of p75_parted for values in (1)",
        ]:
            err, _, _, _ = c.do_sql(stmt)
            check(f"setup: {stmt[:48]}", err is None)

        # --- 1. bare relation reference (subselect.out) -----------------
        err, _, hdr, rows = c.do_sql("select p75_view_a from p75_view_a")
        check("bare relation ref -> (42)",
              err is None and hdr == ["p75_view_a"] and rows == [("(42)",)])

        # --- 2. correlated whole-row, singly and doubly nested ---------
        err, _, hdr, rows = c.do_sql("select (select p75_view_a) from p75_view_a")
        check("correlated whole-row subquery -> (42)",
              err is None and hdr == ["p75_view_a"] and rows == [("(42)",)])
        err, _, _, rows = c.do_sql(
            "select (select (select p75_view_a)) from p75_view_a")
        check("doubly nested whole-row subquery -> (42)",
              err is None and rows == [("(42)",)])

        # --- 3. whole-row in expression position -----------------------
        err, _, hdr, rows = c.do_sql(
            "select (select (a.*)::text) from p75_view_a a")
        check("(a.*)::text -> (42) with header a",
              err is None and hdr == ["a"] and rows == [("(42)",)])
        err, _, _, rows = c.do_sql("select (p75_t.*)::text from p75_t order by a")
        check("(p75_t.*)::text renders composites",
              err is None and rows == [("(1,foo)",), ("(2,)",)])

        # --- 4. derived-table whole-row: no junk columns ---------------
        err, _, hdr, rows = c.do_sql(
            "select q from (select max(f1) from p75_int4 group by f1 order by f1) q")
        check("derived whole-row has one column, five rows",
              err is None and hdr == ["q"] and rows == [
                  ("(-2147483647)",), ("(-123456)",), ("(0)",),
                  ("(123456)",), ("(2147483647)",)])
        err, _, _, rows = c.do_sql(
            "with q as (select max(f1) from p75_int4 group by f1 order by f1) select q from q")
        check("CTE whole-row has one column, five rows",
              err is None and len(rows) == 5 and all(len(r) == 1 for r in rows))

        # --- 5. USING-alias whole-row shape (join.out) -----------------
        err, _, hdr, rows = c.do_sql(
            "select x.* from p75_j1 join p75_j2 using (i) as x")
        check("USING-alias x.* merges to one column",
              err is None and hdr == ["i"] and rows == [("1",)])
        err, _, _, rows = c.do_sql(
            "select row_to_json(x.*) from p75_j1 join p75_j2 using (i) as x")
        check("row_to_json(x.*) -> {\"i\":1}",
              err is None and rows == [('{"i":1}',)])

        # --- 6. INSERT ... RETURNING tbl (insert.out) ------------------
        err, _, hdr, rows = c.do_sql(
            "insert into p75_t values (3,'bar') returning p75_t")
        check("RETURNING tbl -> (3,bar)",
              err is None and hdr == ["p75_t"] and rows == [("(3,bar)",)])
        err, _, _, rows = c.do_sql(
            "insert into p75_parted values (1) returning p75_parted")
        check("partitioned RETURNING tbl -> (1)",
              err is None and rows == [("(1)",)])
        # parent column order wins over the leaf's physical order
        for stmt in [
            "alter table p75_parted add b text",
            "create table p75_parted2 (b text, c int, a int)",
            "alter table p75_parted2 drop c",
            "alter table p75_parted attach partition p75_parted2 for values in (2)",
        ]:
            err, _, _, _ = c.do_sql(stmt)
            check(f"partition setup: {stmt[:40]}", err is None)
        err, _, _, rows = c.do_sql(
            "insert into p75_parted values (2, 'foo') returning p75_parted")
        check("partitioned RETURNING uses parent column order",
              err is None and rows == [("(2,foo)",)])

        # --- 7. aggregate / IS NULL null semantics --------------------
        err, _, _, rows = c.do_sql(
            "select count(p75_t2.*) from p75_t1 left join p75_t2 on p75_t1.a = p75_t2.a")
        check("count(t2.*) skips null-extended row",
              err is None and rows == [("1",)])
        err, _, _, rows = c.do_sql(
            "select count(p75_t1.*) from p75_t1 left join p75_t2 on p75_t1.a = p75_t2.a")
        check("count(t1.*) counts both rows",
              err is None and rows == [("2",)])
        err, _, _, rows = c.do_sql(
            "select (p75_t2.*) is null from p75_t1 left join p75_t2 "
            "on p75_t1.a = p75_t2.a order by p75_t1.a")
        check("(t2.*) IS NULL true exactly for the null-extended row",
              err is None and rows == [("t",), ("f",)])

        # --- 8. errors and precedence are preserved -------------------
        err, _, _, _ = c.do_sql("select nosuchcol from p75_t")
        check("genuinely unknown column still 42703", err == "42703")
        err, _, _, _ = c.do_sql("select nosuch.* from p75_t")
        check("unknown qualifier still fails", err in ("42703", "42P01"))
        # a column beats a range of the same name
        c.do_sql("create table p75_self(p75_self int)")
        c.do_sql("insert into p75_self values (7)")
        err, _, _, rows = c.do_sql("select p75_self from p75_self")
        check("column reference wins over range name",
              err is None and rows == [("7",)])

        c.close()
    finally:
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol 75: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
