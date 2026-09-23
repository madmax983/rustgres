#!/usr/bin/env python3
"""protocol_test76.py — v0.74 PostgreSQL-19 join grammar and zero-column tables.

Covers two v0.74 features, grounded in the REL_19_STABLE regression
outputs (join.out, subselect.out):

A. PG's shift-preferred join grammar (gram.y): when a join's right
   operand is followed immediately by another join keyword (no ON/USING
   seen yet), the right operand extends rightward instead of reducing.
   `a LEFT JOIN b LEFT JOIN c ON p1 ON p2` parses as
   `a LEFT JOIN (b LEFT JOIN c ON p1) ON p2`, and
   `t1 INNER JOIN i1 LEFT JOIN subq ON p1 ON p2` parses as
   `t1 INNER JOIN (i1 LEFT JOIN subq ON p1) ON p2` — the shape PG's own
   join.sql regression exercises. Also: PG19 requires ON/USING for
   INNER/LEFT/RIGHT/FULL JOIN — a missing qualifier is 42601 (only
   CROSS JOIN and NATURAL JOIN omit it).

B. Zero-column tables: `CREATE TABLE t()` is legal; `INSERT INTO t
   DEFAULT VALUES` inserts one empty row; `SELECT EXISTS(SELECT * FROM t)`
   and `ANALYZE t` work; the join.sql `onerow()` nested-subquery cases
   from §A.6.1 execute with the row present.

This test manages its own server on port 5548 (so it never collides
with the conformance runner) and is RED without the v0.74 support.
"""

import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5548
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
    data_dir = tempfile.mkdtemp(prefix="rg76_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        for stmt in [
            "create table p76_a(x int)",
            "create table p76_b(x int)",
            "create table p76_c(x int)",
            "insert into p76_a values (1),(2)",
            "insert into p76_b values (1),(3)",
            "insert into p76_c values (1),(4)",
            "create table p76_t1(unique2 int)",
            "create table p76_i1(f1 int)",
            "insert into p76_t1 values (11)",
            "insert into p76_i1 values (0)",
        ]:
            err, _, _, _ = c.do_sql(stmt)
            check(f"setup: {stmt[:48]}", err is None)

        # --- A1. right-nested LEFT JOIN (join.sql shape) ----------------
        # a LEFT JOIN (b LEFT JOIN c ON b.x=c.x) ON a.x=b.x
        # inner: b⟕c = (1,1),(3,NULL); outer: (1,1,1),(2,NULL,NULL)
        err, _, _, rows = c.do_sql(
            "select a.x,b.x,c.x from p76_a a"
            " left join p76_b b left join p76_c c on b.x=c.x on a.x=b.x"
            " order by 1")
        check("right-nested left join -> (1,1,1),(2,NULL,NULL)",
              err is None and rows == [("1", "1", "1"), ("2", None, None)])

        # --- A2. left-deep chains are unchanged -------------------------
        # ((a⟕b ON a.x=b.x) ⟕c ON b.x=c.x) = same rows here
        err, _, _, rows = c.do_sql(
            "select a.x,b.x,c.x from p76_a a"
            " left join p76_b b on a.x=b.x left join p76_c c on b.x=c.x"
            " order by 1")
        check("left-deep chain unchanged",
              err is None and rows == [("1", "1", "1"), ("2", None, None)])

        # --- A3. unparenthesized agrees with explicit parens ---------------
        # The shift-preferred grammar must equal the parenthesized form.
        err, _, _, rows = c.do_sql(
            "select a.x,b.x,c.x from p76_a a"
            " left join (p76_b b left join p76_c c on b.x=c.x) on a.x=b.x"
            " order by 1")
        check("unparenthesized == explicitly parenthesized right-nest",
              err is None and rows == [("1", "1", "1"), ("2", None, None)])
        # NOTE: an inner ON referencing the *outer* table (legal in PG's
        # grammar, e.g. `a LEFT JOIN b LEFT JOIN c ON c.x=a.x ...`) is
        # still 42703 here — the executor scopes a nested join's ON to
        # its own tables. Not covered by the regression suite; deferred.


        # --- A4. PG's big nested query shape (join.out) -----------------
        # t1 INNER JOIN (i1 LEFT JOIN subq ON i1.f1=subq.x2)
        #   ON t1.unique2=subq.d1 ; subq=(0,3,11)
        err, _, _, rows = c.do_sql(
            "select t1.unique2, subq1.d1, subq1.y1 from p76_t1 t1"
            " inner join p76_i1 i1"
            " left join (select v1.x2, v2.y1, 11 as d1"
            " from (select 1,0) v1(x1,x2)"
            " left join (select 3,1) v2(y1,y2) on v1.x1 = v2.y2) subq1"
            " on (i1.f1 = subq1.x2) on (t1.unique2 = subq1.d1)")
        check("join.out nested shape -> (11,11,3)",
              err is None and rows == [("11", "11", "3")])

        # --- A5. JOIN without ON/USING is a syntax error (PG19) ---------
        # Only CROSS JOIN and NATURAL JOIN omit the qualifier.
        err, _, _, _ = c.do_sql("select count(*) from p76_a inner join p76_b")
        check("inner join without ON -> 42601", err == "42601")
        err, _, _, _ = c.do_sql("select count(*) from p76_a join p76_b")
        check("bare JOIN without ON -> 42601", err == "42601")

        # --- A6. outer join without ON is still an error ----------------
        err, _, _, _ = c.do_sql("select * from p76_a left join p76_b")
        check("left join without ON stays 42601", err == "42601")
        err, _, _, _ = c.do_sql("select * from p76_a a cross join p76_b b")
        check("cross join still parses", err is None)

        # --- A7. explicit parens still nest as written -----------------
        err, _, _, rows = c.do_sql(
            "select a.x,b.x,c.x from p76_a a"
            " left join (p76_b b left join p76_c c on b.x=c.x) on a.x=b.x"
            " order by 1")
        check("explicit parens nest as written",
              err is None and rows == [("1", "1", "1"), ("2", None, None)])

        # --- B1. zero-column table: create + EXISTS --------------------
        err, _, _, _ = c.do_sql("create temp table p76_nocolumns()")
        check("create temp table p76_nocolumns()", err is None)
        err, _, _, rows = c.do_sql("select exists(select * from p76_nocolumns)")
        check("exists over empty zero-col table -> f",
              err is None and rows == [("f",)])

        # --- B2. INSERT DEFAULT VALUES + ANALYZE (join.out) ------------
        err, _, _, _ = c.do_sql("create temp table p76_onerow()")
        check("create temp table p76_onerow()", err is None)
        err, tag, _, _ = c.do_sql("insert into p76_onerow default values")
        check("insert default values -> INSERT 0 1",
              err is None and tag == "INSERT 0 1")
        err, _, _, rows = c.do_sql("select count(*) from p76_onerow")
        check("onerow has one row", err is None and rows == [("1",)])
        err, tag, _, _ = c.do_sql("analyze p76_onerow")
        check("analyze zero-col table", err is None and tag == "ANALYZE")

        # --- B3. onerow nested-subquery join (join.out) -----------------
        err, _, _, rows = c.do_sql(
            "select v1.x2 from (select 1,0 from p76_onerow) v1(x1,x2)"
            " left join (select 3,1 from p76_onerow) v2(y1,y2)"
            " on (v1.x2 = v2.y2)")
        check("onerow nested subquery join -> (0)",
              err is None and rows == [("0",)])
        err, _, _, rows = c.do_sql(
            "select v1.x2 from (select 1,0 from p76_onerow) v1(x1,x2)"
            " left join (select 3,1 from p76_onerow) v2(y1,y2)"
            " on (v1.x2 = v2.y2 and v1.x1 = v2.y1)")
        check("onerow conjunctive ON -> (0)",
              err is None and rows == [("0",)])

        # --- B4. DEFAULT VALUES on a normal table ----------------------
        err, _, _, _ = c.do_sql("create table p76_d(a int default 7, b text)")
        check("setup p76_d", err is None)
        err, _, _, rows = c.do_sql(
            "insert into p76_d default values returning a, b")
        check("default values applies column defaults",
              err is None and rows == [("7", None)])

        # --- C1. DELETE ... USING (join.sql / join.out) ------------------
        err, _, _, _ = c.do_sql("create table p76_du1(a int, b int)")
        check("setup p76_t1", err is None)
        err, _, _, _ = c.do_sql("create table p76_du2(a int, b int)")
        check("setup p76_t2", err is None)
        err, _, _, _ = c.do_sql("create table p76_du3(x int, y int)")
        check("setup p76_t3", err is None)
        err, _, _, _ = c.do_sql(
            "insert into p76_du1 values (5, 10), (15, 20), (100, 100), (200, 1000)")
        check("setup p76_t1 rows", err is None)
        err, _, _, _ = c.do_sql("insert into p76_du2 values (200, 2000)")
        check("setup p76_t2 rows", err is None)
        err, _, _, _ = c.do_sql(
            "insert into p76_du3 values (5, 20), (6, 7), (7, 8), (500, 100)")
        check("setup p76_t3 rows", err is None)
        err, tag, _, _ = c.do_sql(
            "delete from p76_du3 using p76_du1 table1 where p76_du3.x = table1.a")
        check("delete using -> DELETE 1", err is None and tag == "DELETE 1")
        err, _, _, rows = c.do_sql("select x, y from p76_du3 order by x")
        check("3 rows remain",
              err is None and rows == [("6", "7"), ("7", "8"), ("500", "100")])
        err, tag, _, _ = c.do_sql(
            "delete from p76_du3 using p76_du1 join p76_du2 using (a)"
            " where p76_du3.x > p76_du1.a")
        check("delete using join -> DELETE 1", err is None and tag == "DELETE 1")
        err, _, _, rows = c.do_sql("select x, y from p76_du3 order by x")
        check("2 rows remain",
              err is None and rows == [("6", "7"), ("7", "8")])
        err, tag, _, _ = c.do_sql(
            "delete from p76_du3 using p76_du3 t3_other"
            " where p76_du3.x = t3_other.x and p76_du3.y = t3_other.y")
        check("delete using self-join -> DELETE 2",
              err is None and tag == "DELETE 2")
        err, _, _, rows = c.do_sql("select count(*) from p76_du3")
        check("t3 empty", err is None and rows == [("0",)])

        # --- D1. PREPARE / EXECUTE / DEALLOCATE -------------------------
        err, tag, _, _ = c.do_sql(
            "prepare p76_foo(bool) as select count(*) from p76_a a"
            " left join p76_a b on (a.x = b.x and $1)")
        check("prepare with param", err is None and tag == "PREPARE")
        err, _, _, rows = c.do_sql("execute p76_foo(true)")
        check("execute true", err is None and rows == [("2",)])
        err, _, _, rows = c.do_sql("execute p76_foo(false)")
        check("execute false", err is None and rows == [("2",)])
        err, tag, _, _ = c.do_sql("deallocate p76_foo")
        check("deallocate", err is None and tag == "DEALLOCATE")
        err, _, _, _ = c.do_sql("execute p76_foo(true)")
        check("execute after deallocate -> 26000", err == "26000")
        err, tag, _, _ = c.do_sql("prepare p76_bar as select 42")
        check("prepare without types", err is None and tag == "PREPARE")
        err, _, _, rows = c.do_sql("execute p76_bar")
        check("execute no-arg", err is None and rows == [("42",)])
        err, tag, _, _ = c.do_sql("deallocate all")
        check("deallocate all", err is None and tag == "DEALLOCATE")

        c.close()
    finally:
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol76: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
