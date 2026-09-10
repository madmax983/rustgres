#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.10 (CTEs, windows, UPSERT, RETURNING, COPY).

Covers WITH / WITH RECURSIVE (multiple CTEs, aliases, recursive UNION
semantics, references from joins/subqueries/DML), window functions
(row_number/rank/dense_rank/ntile/lag/lead/first_value/last_value/
nth_value, aggregate windows, PARTITION BY / ORDER BY / ROWS/RANGE
frames, ties, NULL ordering), INSERT .. ON CONFLICT (DO NOTHING /
DO UPDATE, EXCLUDED, arbiter inference), RETURNING on INSERT/UPDATE/
DELETE (simple + extended protocol), and COPY FROM STDIN / TO STDOUT
(text/CSV, options, line-numbered errors, protocol messages).

Each test boots a fresh server on a scratch data dir, so state can
never leak between tests.

Usage: build the server first (`cargo build`), then `python3 tests/protocol_test10.py`.
"""
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
HOST = "127.0.0.1"

_next_port = [55743]


def alloc_port():
    _next_port[0] += 1
    return _next_port[0]


passed = []
failed = []


def check(name, cond, detail=""):
    if cond:
        passed.append(name)
        print(f"  ok: {name}")
    else:
        failed.append(name)
        print(f"  FAIL: {name} {detail}")


def msg(typ, payload):
    return typ + struct.pack("!i", len(payload) + 4) + payload


def cstr(s):
    return s.encode() + b"\x00"


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection((HOST, port), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self._drain_until_ready()

    def _read_msg(self):
        t = self.s.recv(1)
        if not t:
            raise RuntimeError("connection closed by server")
        (ln,) = struct.unpack("!i", self._read_exact(4))
        return t, self._read_exact(ln - 4)

    def _read_exact(self, n):
        data = b""
        while len(data) < n:
            chunk = self.s.recv(n - len(data))
            if not chunk:
                raise RuntimeError("connection closed mid-message")
            data += chunk
        return data

    def _drain_until_ready(self):
        while True:
            t, _ = self._read_msg()
            if t == b"Z":
                return

    def q(self, sql):
        """Returns (tags, rows, err_codes). Column names land on
        self.last_columns."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        tags, rows, codes = [], [], []
        self.last_columns = []
        while True:
            t, p = self._read_msg()
            if t == b"C":
                tags.append(p[:-1].decode())
            elif t == b"T":
                (n,) = struct.unpack("!h", p[:2])
                pos, cols = 2, []
                for _ in range(n):
                    end = p.index(b"\x00", pos)
                    cols.append(p[pos:end].decode())
                    pos = end + 1 + 18
                self.last_columns = cols
            elif t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    if ln == -1:
                        r.append(None)
                    else:
                        r.append(p[pos:pos + ln].decode())
                        pos += ln
                rows.append(tuple(r))
            elif t == b"E":
                # SQLSTATE is field 'C'.
                code = ""
                i = 0
                while i < len(p):
                    if p[i:i + 1] == b"C":
                        j = p.index(b"\x00", i + 1)
                        code = p[i + 1:j].decode()
                        break
                    j = p.index(b"\x00", i + 1)
                    i = j + 1
                codes.append(code)
            elif t == b"Z":
                break
            # Ignore H/G/d/c (COPY is tested via copy_to/copy_from).
        return tags, rows, codes

    def copy_to(self, sql):
        """Run COPY .. TO STDOUT. Returns (data_bytes, tag, err_code)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        data = b""
        tag, code = "", ""
        while True:
            t, p = self._read_msg()
            if t == b"H":
                pass  # CopyOutResponse
            elif t == b"d":
                data += p
            elif t == b"c":
                pass  # CopyDone
            elif t == b"C":
                tag = p[:-1].decode()
            elif t == b"E":
                i = 0
                while i < len(p):
                    if p[i:i + 1] == b"C":
                        j = p.index(b"\x00", i + 1)
                        code = p[i + 1:j].decode()
                        break
                    j = p.index(b"\x00", i + 1)
                    i = j + 1
            elif t == b"Z":
                break
        return data, tag, code

    def copy_from(self, sql, data_bytes, fail=False):
        """Run COPY .. FROM STDIN. Returns (tag, err_code)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        tag, code = "", ""
        # Expect CopyInResponse.
        t, p = self._read_msg()
        if t != b"G":
            return ("", "expected CopyInResponse, got %r" % t)
        # Send data in chunks, then CopyDone (or CopyFail).
        for i in range(0, len(data_bytes), 1024):
            self.s.sendall(msg(b"d", data_bytes[i:i + 1024]))
        self.s.sendall(msg(b"f", b"client fail") if fail else msg(b"c", b""))
        while True:
            t, p = self._read_msg()
            if t == b"C":
                tag = p[:-1].decode()
            elif t == b"E":
                i = 0
                while i < len(p):
                    if p[i:i + 1] == b"C":
                        j = p.index(b"\x00", i + 1)
                        code = p[i + 1:j].decode()
                        break
                    j = p.index(b"\x00", i + 1)
                    i = j + 1
            elif t == b"Z":
                break
        return tag, code

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        except OSError:
            pass
        self.s.close()


def fresh_server():
    d = tempfile.mkdtemp(prefix="rg10_")
    port = alloc_port()
    proc = subprocess.Popen(
        [BIN, "--data-dir", d, "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    # Wait for the port to accept.
    for _ in range(100):
        try:
            c = Conn(port)
            return proc, d, c
        except OSError:
            time.sleep(0.05)
    proc.kill()
    raise RuntimeError("server did not start")


def teardown(proc, d, c):
    try:
        c.close()
    except Exception:
        pass
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    shutil.rmtree(d, ignore_errors=True)


# ---------------------------------------------------------------------------
# 1. CTEs
# ---------------------------------------------------------------------------

def test_cte_basic():
    proc, d, c = fresh_server()
    try:
        tags, rows, codes = c.q("WITH t AS (SELECT 1 AS a, 2 AS b) SELECT a, b FROM t")
        check("cte-basic", rows == [("1", "2")], f"{rows} {codes}")
        # Multiple CTEs, later referencing earlier.
        tags, rows, codes = c.q(
            "WITH a AS (SELECT 1 AS x), b AS (SELECT x + 1 AS y FROM a) SELECT y FROM b"
        )
        check("cte-multiple", rows == [("2",)], f"{rows} {codes}")
        # CTE referenced twice.
        tags, rows, codes = c.q(
            "WITH t AS (SELECT 10 AS v) SELECT (SELECT v FROM t) + (SELECT v FROM t)"
        )
        check("cte-twice", rows == [("20",)], f"{rows} {codes}")
        # Column aliases.
        tags, rows, codes = c.q(
            "WITH t(x, y) AS (SELECT 1, 2) SELECT x + y FROM t"
        )
        check("cte-col-alias", rows == [("3",)], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


def test_cte_from_tables():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE emp(id INT, name TEXT, dept INT)")
        c.q("INSERT INTO emp VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 10)")
        # CTE over a table, joined.
        tags, rows, codes = c.q(
            "WITH d10 AS (SELECT * FROM emp WHERE dept = 10) "
            "SELECT name FROM d10 ORDER BY id"
        )
        check("cte-table", rows == [("a",), ("c",)], f"{rows} {codes}")
        # CTE in a subquery / join.
        tags, rows, codes = c.q(
            "WITH x AS (SELECT dept, COUNT(*) AS n FROM emp GROUP BY dept) "
            "SELECT dept FROM x WHERE n > 1"
        )
        check("cte-agg", rows == [("10",)], f"{rows} {codes}")
        # DML reading a CTE.
        tags, rows, codes = c.q(
            "WITH src AS (SELECT 4 AS id, 'd' AS name, 30 AS dept) "
            "INSERT INTO emp SELECT * FROM src RETURNING id"
        )
        check("cte-insert", rows == [("4",)] and tags == ["INSERT 0 1"], f"{rows} {tags} {codes}")
        tags, rows, codes = c.q(
            "WITH old AS (SELECT 2 AS del_id) "
            "DELETE FROM emp WHERE id = 2 RETURNING id"
        )
        check("cte-delete", rows == [("2",)] and tags == ["DELETE 1"], f"{rows} {tags} {codes}")
    finally:
        teardown(proc, d, c)


def test_cte_recursive():
    proc, d, c = fresh_server()
    try:
        # Classic countdown.
        tags, rows, codes = c.q(
            "WITH RECURSIVE r(n) AS (SELECT 3 UNION ALL SELECT n - 1 FROM r WHERE n > 1) "
            "SELECT n FROM r"
        )
        check("cte-rec-countdown", rows == [("3",), ("2",), ("1",)], f"{rows} {codes}")
        # UNION (distinct) suppresses duplicates.
        tags, rows, codes = c.q(
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT 1) SELECT n FROM r"
        )
        check("cte-rec-union-distinct", rows == [("1",)], f"{rows} {codes}")
        # Fibonacci-ish expansion terminates.
        tags, rows, codes = c.q(
            "WITH RECURSIVE f(a, b) AS ("
            " SELECT 1, 1 UNION ALL SELECT b, a + b FROM f WHERE a < 10"
            ") SELECT a FROM f ORDER BY a"
        )
        check("cte-rec-fib", rows == [("1",), ("1",), ("2",), ("3",), ("5",), ("8",), ("13",)], f"{rows} {codes}")
        # Recursive CTE used in a join.
        tags, rows, codes = c.q(
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) "
            "SELECT n * 10 FROM r ORDER BY n"
        )
        check("cte-rec-join", rows == [("10",), ("20",), ("30",)], f"{rows} {codes}")
        # Non-recursive WITH RECURSIVE still works.
        tags, rows, codes = c.q(
            "WITH RECURSIVE t AS (SELECT 42 AS x) SELECT x FROM t"
        )
        check("cte-recursive-nonrec", rows == [("42",)], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


# ---------------------------------------------------------------------------
# 2. Window functions
# ---------------------------------------------------------------------------

def test_window_basics():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE w(a INT, b INT)")
        c.q("INSERT INTO w VALUES (1, 10), (1, 20), (2, 30), (2, 40), (2, 50)")
        tags, rows, codes = c.q(
            "SELECT a, b, row_number() OVER (PARTITION BY a ORDER BY b) AS rn FROM w ORDER BY a, b"
        )
        check("win-row_number", rows == [
            ("1", "10", "1"), ("1", "20", "2"),
            ("2", "30", "1"), ("2", "40", "2"), ("2", "50", "3"),
        ], f"{rows} {codes}")
        tags, rows, codes = c.q(
            "SELECT rank() OVER (ORDER BY b), dense_rank() OVER (ORDER BY b) FROM w ORDER BY b"
        )
        check("win-rank-dense", rows == [
            ("1", "1"), ("2", "2"), ("3", "3"), ("4", "4"), ("5", "5"),
        ], f"{rows} {codes}")
        # Ties: rank gaps, dense_rank does not.
        c.q("CREATE TABLE wt(g INT, v INT)")
        c.q("INSERT INTO wt VALUES (1, 10), (1, 10), (1, 20)")
        tags, rows, codes = c.q(
            "SELECT v, rank() OVER (ORDER BY v), dense_rank() OVER (ORDER BY v) FROM wt ORDER BY v"
        )
        check("win-ties", rows == [
            ("10", "1", "1"), ("10", "1", "1"), ("20", "3", "2"),
        ], f"{rows} {codes}")
        # Aggregates as windows.
        tags, rows, codes = c.q(
            "SELECT a, SUM(b) OVER (PARTITION BY a), COUNT(*) OVER (PARTITION BY a), "
            "AVG(b) OVER (PARTITION BY a) FROM w ORDER BY a, b"
        )
        check("win-agg", rows == [
            ("1", "30", "2", "15"), ("1", "30", "2", "15"),
            ("2", "120", "3", "40"), ("2", "120", "3", "40"), ("2", "120", "3", "40"),
        ], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


def test_window_nav():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE w(b INT)")
        c.q("INSERT INTO w VALUES (10), (20), (30)")
        tags, rows, codes = c.q(
            "SELECT b, lag(b) OVER (ORDER BY b), lead(b) OVER (ORDER BY b) FROM w ORDER BY b"
        )
        check("win-lag-lead", rows == [
            ("10", None, "20"), ("20", "10", "30"), ("30", "20", None),
        ], f"{rows} {codes}")
        # Offsets and defaults.
        tags, rows, codes = c.q(
            "SELECT lag(b, 2, -1) OVER (ORDER BY b), lead(b, 2, -1) OVER (ORDER BY b) "
            "FROM w ORDER BY b"
        )
        check("win-lag-lead-args", rows == [
            ("-1", "30"), ("-1", "-1"), ("10", "-1"),
        ], f"{rows} {codes}")
        # first/last/nth value.
        tags, rows, codes = c.q(
            "SELECT first_value(b) OVER (ORDER BY b), "
            "last_value(b) OVER (ORDER BY b ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING), "
            "nth_value(b, 2) OVER (ORDER BY b ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) "
            "FROM w ORDER BY b"
        )
        check("win-first-last-nth", rows == [
            ("10", "30", "20"), ("10", "30", "20"), ("10", "30", "20"),
        ], f"{rows} {codes}")
        # ntile.
        tags, rows, codes = c.q(
            "SELECT b, ntile(2) OVER (ORDER BY b) FROM w ORDER BY b"
        )
        check("win-ntile", rows == [("10", "1"), ("20", "1"), ("30", "2")], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


def test_window_frames():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE w(b INT)")
        c.q("INSERT INTO w VALUES (10), (20), (30), (40)")
        # ROWS frame: running sum.
        tags, rows, codes = c.q(
            "SELECT b, SUM(b) OVER (ORDER BY b ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) "
            "FROM w ORDER BY b"
        )
        check("win-rows-running", rows == [
            ("10", "10"), ("20", "30"), ("30", "60"), ("40", "100"),
        ], f"{rows} {codes}")
        # ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING.
        tags, rows, codes = c.q(
            "SELECT b, SUM(b) OVER (ORDER BY b ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) "
            "FROM w ORDER BY b"
        )
        check("win-rows-sliding", rows == [
            ("10", "30"), ("20", "60"), ("30", "90"), ("40", "70"),
        ], f"{rows} {codes}")
        # Default frame with ties expands to peers (RANGE).
        c.q("CREATE TABLE w2(b INT)")
        c.q("INSERT INTO w2 VALUES (10), (10), (20)")
        tags, rows, codes = c.q(
            "SELECT b, SUM(b) OVER (ORDER BY b) FROM w2 ORDER BY b"
        )
        check("win-range-peer", rows == [
            ("10", "20"), ("10", "20"), ("20", "40"),
        ], f"{rows} {codes}")
        # RANGE with numeric offset.
        tags, rows, codes = c.q(
            "SELECT b, COUNT(*) OVER (ORDER BY b RANGE BETWEEN 10 PRECEDING AND 10 FOLLOWING) "
            "FROM w ORDER BY b"
        )
        check("win-range-offset", rows == [
            ("10", "2"), ("20", "3"), ("30", "3"), ("40", "2"),
        ], f"{rows} {codes}")
        # Default RANGE on text ORDER BY (peer expansion, no numeric coercion).
        c.q("CREATE TABLE w3(t TEXT)")
        c.q("INSERT INTO w3 VALUES ('a'), ('a'), ('b')")
        tags, rows, codes = c.q(
            "SELECT t, COUNT(*) OVER (ORDER BY t) FROM w3 ORDER BY t"
        )
        check("win-range-text", rows == [
            ("a", "2"), ("a", "2"), ("b", "3"),
        ], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


def test_window_nulls_order():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE w(b INT)")
        c.q("INSERT INTO w VALUES (NULL), (10), (NULL), (20)")
        # Default: NULLS LAST for ASC.
        tags, rows, codes = c.q(
            "SELECT b, row_number() OVER (ORDER BY b) FROM w ORDER BY b"
        )
        # NULLs sort last by default.
        check("win-nulls-default", [r[0] for r in rows] == ["10", "20", None, None],
              f"{rows} {codes}")
        # Explicit NULLS FIRST.
        tags, rows, codes = c.q(
            "SELECT b, row_number() OVER (ORDER BY b NULLS FIRST) FROM w ORDER BY b NULLS FIRST"
        )
        check("win-nulls-first", [r[0] for r in rows] == [None, None, "10", "20"],
              f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


def test_window_grouped_having():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE w(g INT, v INT)")
        c.q("INSERT INTO w VALUES (1, 10), (1, 20), (2, 30), (3, 40)")
        # Windows see only groups surviving HAVING.
        tags, rows, codes = c.q(
            "SELECT g, SUM(v) AS s, rank() OVER (ORDER BY SUM(v)) "
            "FROM w GROUP BY g HAVING SUM(v) > 15 ORDER BY g"
        )
        # Groups: g=1 (30), g=2 (30), g=3 (40). All survive (30>15, 40>15).
        # rank over (30, 30, 40): 1, 1, 3.
        check("win-having", rows == [
            ("1", "30", "1"), ("2", "30", "1"), ("3", "40", "3"),
        ], f"{rows} {codes}")
        # Stricter HAVING: only g=3 survives; rank must be 1 (not 3).
        tags, rows, codes = c.q(
            "SELECT g, rank() OVER (ORDER BY SUM(v)) "
            "FROM w GROUP BY g HAVING SUM(v) > 35 ORDER BY g"
        )
        check("win-having-filter", rows == [("3", "1")], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


# ---------------------------------------------------------------------------
# 3. UPSERT
# ---------------------------------------------------------------------------

def test_upsert():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE u(id INT PRIMARY KEY, v TEXT)")
        c.q("INSERT INTO u VALUES (1, 'a'), (2, 'b')")
        # DO NOTHING on conflict.
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES (1, 'x') ON CONFLICT (id) DO NOTHING"
        )
        check("upsert-do-nothing", tags == ["INSERT 0 0"], f"{tags} {codes}")
        tags, rows, codes = c.q("SELECT v FROM u WHERE id = 1")
        check("upsert-do-nothing-kept", rows == [("a",)], f"{rows}")
        # DO UPDATE with EXCLUDED.
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v"
        )
        check("upsert-do-update", tags == ["INSERT 0 1"], f"{tags} {codes}")
        tags, rows, codes = c.q("SELECT v FROM u WHERE id = 1")
        check("upsert-do-update-applied", rows == [("x",)], f"{rows}")
        # Non-conflicting row inserts normally.
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES (3, 'c') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v"
        )
        check("upsert-no-conflict", tags == ["INSERT 0 1"], f"{tags} {codes}")
        # Multi-row with mixed conflict/new.
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES (2, 'B'), (4, 'd') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v"
        )
        check("upsert-mixed", tags == ["INSERT 0 2"], f"{tags} {codes}")
        tags, rows, codes = c.q("SELECT id, v FROM u ORDER BY id")
        check("upsert-mixed-rows", rows == [
            ("1", "x"), ("2", "B"), ("3", "c"), ("4", "d"),
        ], f"{rows}")
        # ON CONFLICT DO NOTHING without arbiter.
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES (1, 'z') ON CONFLICT DO NOTHING"
        )
        check("upsert-no-arbiter", tags == ["INSERT 0 0"], f"{tags} {codes}")
    finally:
        teardown(proc, d, c)


def test_upsert_unique_index():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE u(email TEXT UNIQUE, n INT)")
        c.q("INSERT INTO u VALUES ('a@x', 1)")
        # Conflict inferred from unique index.
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES ('a@x', 2) ON CONFLICT (email) DO UPDATE SET n = EXCLUDED.n"
        )
        check("upsert-unique-index", tags == ["INSERT 0 1"], f"{tags} {codes}")
        tags, rows, codes = c.q("SELECT n FROM u WHERE email = 'a@x'")
        check("upsert-unique-applied", rows == [("2",)], f"{rows}")
        # WHERE clause on DO UPDATE.
        c.q("INSERT INTO u VALUES ('b@x', 10)")
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES ('b@x', 20) ON CONFLICT (email) DO UPDATE "
            "SET n = EXCLUDED.n WHERE u.n < 15"
        )
        tags, rows, codes = c.q("SELECT n FROM u WHERE email = 'b@x'")
        check("upsert-where-true", rows == [("20",)], f"{rows}")
        tags, rows, codes = c.q(
            "INSERT INTO u VALUES ('b@x', 30) ON CONFLICT (email) DO UPDATE "
            "SET n = EXCLUDED.n WHERE u.n < 15"
        )
        tags, rows, codes = c.q("SELECT n FROM u WHERE email = 'b@x'")
        check("upsert-where-false", rows == [("20",)], f"{rows} {tags}")
    finally:
        teardown(proc, d, c)


# ---------------------------------------------------------------------------
# 4. RETURNING
# ---------------------------------------------------------------------------

def test_returning():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE r(id INT, v TEXT)")
        # INSERT .. RETURNING.
        tags, rows, codes = c.q(
            "INSERT INTO r VALUES (1, 'a'), (2, 'b') RETURNING id, v"
        )
        check("ret-insert", rows == [("1", "a"), ("2", "b")] and tags == ["INSERT 0 2"],
              f"{rows} {tags} {codes}")
        check("ret-insert-cols", c.last_columns == ["id", "v"], f"{c.last_columns}")
        # UPDATE .. RETURNING.
        tags, rows, codes = c.q(
            "UPDATE r SET v = 'z' WHERE id = 1 RETURNING id, v"
        )
        check("ret-update", rows == [("1", "z")] and tags == ["UPDATE 1"],
              f"{rows} {tags} {codes}")
        # DELETE .. RETURNING.
        tags, rows, codes = c.q("DELETE FROM r WHERE id = 2 RETURNING id")
        check("ret-delete", rows == [("2",)] and tags == ["DELETE 1"],
              f"{rows} {tags} {codes}")
        # RETURNING with expressions.
        tags, rows, codes = c.q(
            "INSERT INTO r VALUES (3, 'c') RETURNING id + 100, upper(v)"
        )
        check("ret-expr", rows == [("103", "C")], f"{rows} {codes}")
        # Empty RETURNING still sends RowDescription (columns metadata).
        tags, rows, codes = c.q("DELETE FROM r WHERE id = 999 RETURNING id")
        check("ret-empty", rows == [] and tags == ["DELETE 0"] and c.last_columns == ["id"],
              f"{rows} {tags} {c.last_columns} {codes}")
        # UPSERT .. RETURNING.
        c.q("CREATE TABLE ru(id INT PRIMARY KEY, v INT)")
        tags, rows, codes = c.q(
            "INSERT INTO ru VALUES (1, 10) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v RETURNING v"
        )
        check("ret-upsert", rows == [("10",)], f"{rows} {codes}")
    finally:
        teardown(proc, d, c)


def test_returning_extended():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE r(id INT, v TEXT)")
        # Extended protocol: Parse/Bind/Describe/Execute with RETURNING.
        s = c.s
        # Parse.
        parse_body = cstr("stmt1") + cstr("INSERT INTO r VALUES ($1, $2) RETURNING id") + struct.pack("!h", 0)
        s.sendall(msg(b"P", parse_body))
        # Describe statement.
        s.sendall(msg(b"D", b"S" + cstr("stmt1")))
        # Bind.
        bind = cstr("portal1") + cstr("stmt1") + struct.pack("!h", 0) + struct.pack("!h", 2)
        bind += struct.pack("!i", 1) + b"5" + struct.pack("!i", 3) + b"abc"
        bind += struct.pack("!h", 0)
        s.sendall(msg(b"B", bind))
        # Execute.
        s.sendall(msg(b"E", cstr("portal1") + struct.pack("!i", 0)))
        s.sendall(msg(b"S", b""))
        tags, rows = [], []
        while True:
            t, p = c._read_msg()
            if t == b"C":
                tags.append(p[:-1].decode())
            elif t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    r.append(None if ln == -1 else p[pos:pos + ln].decode())
                    pos += max(ln, 0)
                rows.append(tuple(r))
            elif t == b"Z":
                break
        check("ret-extended", rows == [("5",)] and tags == ["INSERT 0 1"],
              f"{rows} {tags}")
    finally:
        teardown(proc, d, c)


# ---------------------------------------------------------------------------
# 5. COPY
# ---------------------------------------------------------------------------

def test_copy_to():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE t(a INT, b TEXT)")
        c.q("INSERT INTO t VALUES (1, 'hello'), (2, NULL), (3, 'axb')")
        # Text format.
        data, tag, code = c.copy_to("COPY t TO STDOUT")
        check("copy-to-text", data == b"1\thello\n2\t\\N\n3\taxb\n" and tag == "COPY 3",
              f"{data!r} {tag} {code}")
        # CSV format (comma delimiter, empty NULL).
        data, tag, code = c.copy_to("COPY t TO STDOUT WITH (FORMAT CSV)")
        check("copy-to-csv", data == b"1,hello\n2,\n3,axb\n" and tag == "COPY 3",
              f"{data!r} {tag} {code}")
        # Column subset.
        data, tag, code = c.copy_to("COPY t (b) TO STDOUT")
        check("copy-to-cols", data == b"hello\n\\N\naxb\n" and tag == "COPY 3",
              f"{data!r} {tag} {code}")
        # HEADER.
        data, tag, code = c.copy_to("COPY t TO STDOUT WITH (FORMAT CSV, HEADER)")
        check("copy-to-header", data == b"a,b\n1,hello\n2,\n3,axb\n" and tag == "COPY 3",
              f"{data!r} {tag} {code}")
        # Custom delimiter and NULL.
        data, tag, code = c.copy_to("COPY t TO STDOUT WITH (DELIMITER '|', NULL 'NULL')")
        check("copy-to-opts", data == b"1|hello\n2|NULL\n3|axb\n" and tag == "COPY 3",
              f"{data!r} {tag} {code}")
        # Missing table.
        data, tag, code = c.copy_to("COPY nope TO STDOUT")
        check("copy-to-missing", code == "42P01", f"{code} {tag}")
    finally:
        teardown(proc, d, c)


def test_copy_from():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE t(a INT, b TEXT)")
        # Text format.
        tag, code = c.copy_from("COPY t FROM STDIN", b"1\thello\n2\t\\N\n3\tworld\n")
        check("copy-from-text", tag == "COPY 3", f"{tag} {code}")
        tags, rows, codes = c.q("SELECT a, b FROM t ORDER BY a")
        check("copy-from-text-rows", rows == [("1", "hello"), ("2", None), ("3", "world")],
              f"{rows} {codes}")
        # CSV format.
        tag, code = c.copy_from("COPY t FROM STDIN WITH (FORMAT CSV)", b'4,"x,y"\n5,\n')
        check("copy-from-csv", tag == "COPY 2", f"{tag} {code}")
        tags, rows, codes = c.q("SELECT a, b FROM t WHERE a >= 4 ORDER BY a")
        check("copy-from-csv-rows", rows == [("4", "x,y"), ("5", None)], f"{rows} {codes}")
        # Column list.
        tag, code = c.copy_from("COPY t (b) FROM STDIN", b"only-b\n")
        check("copy-from-cols", tag == "COPY 1", f"{tag} {code}")
        tags, rows, codes = c.q("SELECT a, b FROM t WHERE b = 'only-b'")
        check("copy-from-cols-rows", rows == [(None, "only-b")], f"{rows}")
        # Malformed: wrong column count -> line-numbered 22P04.
        tag, code = c.copy_from("COPY t FROM STDIN", b"1\tok\n2\n3\tok\n")
        check("copy-from-bad-count", code == "22P04", f"{tag} {code}")
        # Nothing was inserted (statement-atomic).
        tags, rows, codes = c.q("SELECT COUNT(*) FROM t")
        check("copy-from-atomic", rows == [("6",)], f"{rows}")
        # Bad integer -> error, atomic.
        tag, code = c.copy_from("COPY t FROM STDIN", b"notanint\tx\n")
        check("copy-from-bad-int", code != "", f"{tag} {code}")
        tags, rows, codes = c.q("SELECT COUNT(*) FROM t")
        check("copy-from-bad-int-atomic", rows == [("6",)], f"{rows}")
    finally:
        teardown(proc, d, c)


def test_copy_txn():
    proc, d, c = fresh_server()
    try:
        c.q("CREATE TABLE t(a INT)")
        # COPY FROM inside a transaction commits with it.
        c.q("BEGIN")
        tag, code = c.copy_from("COPY t FROM STDIN", b"1\n2\n")
        check("copy-from-txn-tag", tag == "COPY 2", f"{tag} {code}")
        c.q("COMMIT")
        tags, rows, codes = c.q("SELECT COUNT(*) FROM t")
        check("copy-from-txn-commit", rows == [("2",)], f"{rows}")
        # ROLLBACK undoes it.
        c.q("BEGIN")
        tag, code = c.copy_from("COPY t FROM STDIN", b"3\n")
        c.q("ROLLBACK")
        tags, rows, codes = c.q("SELECT COUNT(*) FROM t")
        check("copy-from-txn-rollback", rows == [("2",)], f"{rows}")
        # COPY TO sees transactional data.
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (99)")
        data, tag, code = c.copy_to("COPY t TO STDOUT")
        c.q("ROLLBACK")
        check("copy-to-txn", b"99\n" in data, f"{data!r}")
    finally:
        teardown(proc, d, c)


def main():
    print("== CTEs ==")
    test_cte_basic()
    test_cte_from_tables()
    test_cte_recursive()
    print("== Window functions ==")
    test_window_basics()
    test_window_nav()
    test_window_frames()
    test_window_nulls_order()
    test_window_grouped_having()
    print("== UPSERT ==")
    test_upsert()
    test_upsert_unique_index()
    print("== RETURNING ==")
    test_returning()
    test_returning_extended()
    print("== COPY ==")
    test_copy_to()
    test_copy_from()
    test_copy_txn()
    print()
    print(f"passed {len(passed)}, failed {len(failed)}")
    if failed:
        print("FAILURES:")
        for f in failed:
            print(f"  - {f}")
        sys.exit(1)


if __name__ == "__main__":
    main()
