#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.8 (B-tree indexes, planner, ANALYZE, EXPLAIN).

Covers CREATE/DROP INDEX (incl. IF NOT EXISTS / IF EXISTS and SQLSTATEs),
UNIQUE enforcement (statement-atomic multi-row checks, NULL semantics,
UPDATE conflicts), index access paths (equality, range, BETWEEN, composite
prefixes, ORDER BY ... LIMIT eliding the sort), the text EXPLAIN plan
(incl. the EXPLAIN command tag), ANALYZE statistics and the pg_stats
catalog view, transactional DDL (rollback of CREATE/DROP INDEX), MVCC
visibility of index scans, WAL/checkpoint durability across kill -9, and
an extended-protocol EXPLAIN.

Each test boots a fresh server on a scratch data dir (like the v0.7
suite), so state can never leak between tests.

Usage: build the server first (`cargo build`), then `python3 tests/protocol_test8.py`.
"""
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
HOST = "127.0.0.1"

_next_port = [55633]


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
                raise RuntimeError("connection closed by server")
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
                    pos += ln if ln != -1 else 0
                rows.append(r)
            elif t == b"E":
                fields = {}
                pos = 0
                while p[pos] != 0:
                    ftype = chr(p[pos])
                    end = p.index(b"\x00", pos + 1)
                    fields[ftype] = p[pos + 1:end].decode()
                    pos = end + 1
                codes.append(fields.get("C", "?"))
            elif t == b"Z":
                return tags, rows, codes

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        finally:
            self.s.close()


class XConn(Conn):
    """Extended-protocol bits for the EXPLAIN-over-extended test."""

    def send(self, typ, body):
        self.s.sendall(typ + struct.pack("!i", len(body) + 4) + body)

    def parse(self, name, query):
        self.send(b"P", cstr(name) + cstr(query) + struct.pack("!h", 0))

    def bind(self, portal, stmt):
        self.send(b"B", cstr(portal) + cstr(stmt)
                  + struct.pack("!h", 0) + struct.pack("!h", 0)
                  + struct.pack("!h", 0))

    def describe(self, kind, name):
        self.send(b"D", kind.encode() + cstr(name))

    def execute(self, portal, max_rows=0):
        self.send(b"E", cstr(portal) + struct.pack("!i", max_rows))

    def sync(self):
        self.send(b"S", b"")

    def run_extended(self, query):
        """Parse/Bind/Describe/Execute/Sync; returns (tags, rows, codes)."""
        self.parse("s1", query)
        self.bind("p1", "s1")
        self.describe("P", "p1")
        self.execute("p1")
        self.sync()
        tags, rows, codes = [], [], []
        while True:
            t, p = self._read_msg()
            if t == b"C":
                tags.append(p[:-1].decode())
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
                    pos += ln if ln != -1 else 0
                rows.append(r)
            elif t == b"E":
                fields = {}
                pos = 0
                while p[pos] != 0:
                    ftype = chr(p[pos])
                    end = p.index(b"\x00", pos + 1)
                    fields[ftype] = p[pos + 1:end].decode()
                    pos = end + 1
                codes.append(fields.get("C", "?"))
                # swallow to sync
            elif t == b"Z":
                return tags, rows, codes


def wait_for_port(port, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            s = socket.create_connection((HOST, port), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.1)
    return False


def wait_for_port_free(port, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            s = socket.create_connection((HOST, port), timeout=1)
            s.close()
            time.sleep(0.1)
        except OSError:
            return True
    return False


class Server:
    def __init__(self, port, data_dir=None):
        self.data_dir = data_dir or tempfile.mkdtemp(prefix="rg8_")
        self.port = port
        self.proc = None
        self.own_dir = data_dir is None

    def start(self):
        env = dict(os.environ, RUSTGRES_DATA_DIR=self.data_dir)
        self.proc = subprocess.Popen(
            [BIN, f"--port={self.port}"], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if not wait_for_port(self.port):
            raise RuntimeError(f"server did not open 127.0.0.1:{self.port}")
        time.sleep(0.2)

    def stop(self):
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
            self.proc = None
            wait_for_port_free(self.port)

    def kill9(self):
        if self.proc is not None:
            self.proc.send_signal(signal.SIGKILL)
            self.proc.wait(timeout=10)
            self.proc = None
            wait_for_port_free(self.port)

    def cleanup(self):
        self.stop()
        if self.own_dir:
            shutil.rmtree(self.data_dir, ignore_errors=True)


def fresh_server():
    srv = Server(alloc_port())
    srv.start()
    return srv


def plan_of(conn, sql):
    """EXPLAIN helper: returns the plan lines as one string."""
    tags, rows, codes = conn.q("EXPLAIN " + sql)
    assert codes == [], f"EXPLAIN failed: {codes}"
    assert tags == ["EXPLAIN"], f"bad EXPLAIN tag: {tags}"
    assert conn.last_columns == ["QUERY PLAN"], f"bad columns: {conn.last_columns}"
    assert rows, "EXPLAIN returned no plan lines"
    return "\n".join(r[0] for r in rows)


# ---------------------------------------------------------------------------
# A. CREATE/DROP INDEX DDL
# ---------------------------------------------------------------------------

def t_ddl_basics():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        tags, rows, codes = c.q("CREATE TABLE t(a int, b text)")
        check("create table", tags == ["CREATE TABLE"] and codes == [], f"{tags} {codes}")

        tags, _, codes = c.q("CREATE INDEX idx_a ON t(a)")
        check("create index tag", tags == ["CREATE INDEX"] and codes == [], f"{tags} {codes}")

        _, _, codes = c.q("CREATE INDEX idx_a ON t(a)")
        check("duplicate index name -> 42P07", codes == ["42P07"], f"{codes}")

        tags, _, codes = c.q("CREATE INDEX IF NOT EXISTS idx_a ON t(a)")
        check("if not exists dup -> tag, no error", tags == ["CREATE INDEX"] and codes == [], f"{tags} {codes}")

        _, _, codes = c.q("CREATE INDEX idx_x ON missing(a)")
        check("index on missing table -> 42P01", codes == ["42P01"], f"{codes}")

        _, _, codes = c.q("CREATE INDEX idx_x ON t(nope)")
        check("index on missing column -> 42703", codes == ["42703"], f"{codes}")

        _, _, codes = c.q("CREATE INDEX idx_x ON t(a, a)")
        check("duplicate column -> 42701", codes == ["42701"], f"{codes}")

        tags, _, codes = c.q("CREATE UNIQUE INDEX uq_a ON t(a)")
        check("create unique index", tags == ["CREATE INDEX"] and codes == [], f"{tags} {codes}")

        tags, _, codes = c.q("DROP INDEX idx_a")
        check("drop index tag", tags == ["DROP INDEX"] and codes == [], f"{tags} {codes}")

        _, _, codes = c.q("DROP INDEX idx_a")
        check("drop missing index -> 42P01", codes == ["42P01"], f"{codes}")

        tags, _, codes = c.q("DROP INDEX IF EXISTS idx_a")
        check("drop if exists missing -> tag", tags == ["DROP INDEX"] and codes == [], f"{tags} {codes}")

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# B. UNIQUE enforcement
# ---------------------------------------------------------------------------

def t_unique_enforcement():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE u(a int, b text)")
        c.q("INSERT INTO u VALUES (1,'x'),(2,'y'),(3,NULL),(4,NULL)")
        c.q("CREATE UNIQUE INDEX uq_a ON u(a)")

        _, _, codes = c.q("INSERT INTO u VALUES (2,'dup')")
        check("insert conflicting unique -> 23505", codes == ["23505"], f"{codes}")
        _, rows, _ = c.q("SELECT count(*) FROM u")
        check("conflicting insert inserted nothing", rows == [["4"]], f"{rows}")

        _, _, codes = c.q("INSERT INTO u VALUES (5,'ok'),(6,'ok2'),(2,'boom')")
        check("multi-row insert one conflict -> 23505", codes == ["23505"], f"{codes}")
        _, rows, _ = c.q("SELECT count(*) FROM u")
        check("multi-row insert atomic (no partial rows)", rows == [["4"]], f"{rows}")

        _, _, codes = c.q("INSERT INTO u VALUES (5,'n1'),(5,'n2')")
        check("same-statement duplicate keys -> 23505", codes == ["23505"], f"{codes}")
        _, rows, _ = c.q("SELECT count(*) FROM u")
        check("same-statement dup inserted nothing", rows == [["4"]], f"{rows}")

        tags, _, codes = c.q("INSERT INTO u VALUES (5,NULL)")
        check("NULL unique keys do not conflict", codes == [] and tags == ["INSERT 0 1"], f"{tags} {codes}")

        _, _, codes = c.q("UPDATE u SET a = 2 WHERE a = 1")
        check("update to conflicting value -> 23505", codes == ["23505"], f"{codes}")
        _, rows, _ = c.q("SELECT b FROM u WHERE a = 1")
        check("failed update left old value", rows == [["x"]], f"{rows}")

        c.q("INSERT INTO u VALUES (7,'dup'),(8,'dup')")
        _, _, codes = c.q("UPDATE u SET a = 9 WHERE b = 'dup'")
        check("update two rows to same key -> 23505", codes == ["23505"], f"{codes}")
        _, rows, _ = c.q("SELECT a FROM u WHERE b = 'dup' ORDER BY a")
        check("failed multi-update changed nothing", rows == [["7"], ["8"]], f"{rows}")

        tags, _, codes = c.q("UPDATE u SET b = 'same' WHERE a = 1")
        check("update non-unique col fine", tags == ["UPDATE 1"] and codes == [], f"{tags} {codes}")

        tags, _, codes = c.q("UPDATE u SET a = 1 WHERE a = 1")
        check("update row to its own key fine", tags == ["UPDATE 1"] and codes == [], f"{tags} {codes}")

        # unique index creation over duplicate data must fail
        c.q("CREATE TABLE d(a int)")
        c.q("INSERT INTO d VALUES (1),(1)")
        _, _, codes = c.q("CREATE UNIQUE INDEX uq_d ON d(a)")
        check("create unique over dup data -> 23505", codes == ["23505"], f"{codes}")
        _, _, codes = c.q("DROP INDEX uq_d")
        check("failed create unique left no index", codes == ["42P01"], f"{codes}")

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# C. Index access paths
# ---------------------------------------------------------------------------

def t_index_scans():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE t(a int, b int, c text)")
        vals = ",".join(f"({i},{i % 7},'t{i}')" for i in range(1, 101))
        c.q(f"INSERT INTO t VALUES {vals}")
        c.q("CREATE INDEX idx_a ON t(a)")
        c.q("CREATE INDEX idx_ab ON t(a, b)")

        plan = plan_of(c, "SELECT * FROM t WHERE a = 42")
        check("equality picks index scan",
              "Index Scan using idx_a on t" in plan and "Index Cond: (a = 42)" in plan, plan)
        _, rows, _ = c.q("SELECT * FROM t WHERE a = 42")
        check("equality returns right row", rows == [["42", "0", "t42"]], f"{rows}")

        plan = plan_of(c, "SELECT * FROM t WHERE a > 90 AND a < 95")
        check("range picks index scan",
              "Index Scan using idx_a" in plan and "a > 90" in plan and "a < 95" in plan, plan)
        _, rows, _ = c.q("SELECT * FROM t WHERE a > 90 AND a < 95 ORDER BY a")
        check("range returns right rows", [r[0] for r in rows] == ["91", "92", "93", "94"], f"{rows}")

        plan = plan_of(c, "SELECT * FROM t WHERE a BETWEEN 10 AND 12")
        check("between picks index scan", "Index Scan using idx_a" in plan, plan)
        _, rows, _ = c.q("SELECT * FROM t WHERE a BETWEEN 10 AND 12 ORDER BY a")
        check("between returns right rows", [r[0] for r in rows] == ["10", "11", "12"], f"{rows}")

        plan = plan_of(c, "SELECT * FROM t WHERE a = 5 AND b = 5")
        check("composite prefix equality",
              "Index Scan using idx_ab" in plan and "a = 5" in plan and "b = 5" in plan, plan)

        plan = plan_of(c, "SELECT * FROM t WHERE a = 5 AND b > 3")
        check("composite equality + range",
              "Index Scan using idx_ab" in plan and "b > 3" in plan, plan)
        _, rows, _ = c.q("SELECT b FROM t WHERE a = 5 AND b > 3 ORDER BY b")
        check("composite range rows", rows == [["5"]], f"{rows}")

        plan = plan_of(c, "SELECT * FROM t WHERE c = 't1'")
        check("no index -> seq scan", "Seq Scan on t" in plan and "Index Scan" not in plan, plan)

        plan = plan_of(c, "SELECT * FROM t WHERE a = 1 OR a = 2")
        check("OR predicate -> seq scan", "Seq Scan on t" in plan, plan)

        plan = plan_of(c, "SELECT * FROM t WHERE a <> 5")
        check("<> predicate -> seq scan", "Seq Scan on t" in plan, plan)

        # flipped comparison: literal on the left
        plan = plan_of(c, "SELECT * FROM t WHERE 42 = a")
        check("flipped equality picks index", "Index Scan using idx_a" in plan, plan)

        # correctness cross-check: same results with and without the index
        queries = [
            "SELECT a, b FROM t WHERE a = 7",
            "SELECT a FROM t WHERE a >= 95 ORDER BY a",
            "SELECT count(*) FROM t WHERE a BETWEEN 20 AND 80",
            "SELECT a, b FROM t WHERE a = 50 AND b < 6",
            "SELECT max(b) FROM t WHERE a < 10",
        ]
        with_idx = [c.q(q)[1] for q in queries]
        c.q("DROP INDEX idx_a")
        c.q("DROP INDEX idx_ab")
        without_idx = [c.q(q)[1] for q in queries]
        check("index/no-index results identical", with_idx == without_idx,
              f"{with_idx} vs {without_idx}")

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# D. ORDER BY via index
# ---------------------------------------------------------------------------

def t_order_by_index():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE t(a int, b text)")
        c.q("INSERT INTO t VALUES (3,'c'),(1,'a'),(2,'b'),(NULL,'n')")
        c.q("CREATE INDEX idx_a ON t(a)")

        plan = plan_of(c, "SELECT a FROM t ORDER BY a LIMIT 2")
        check("order by limit uses index scan",
              "Index Scan using idx_a" in plan and "Sort" not in plan, plan)
        _, rows, _ = c.q("SELECT a FROM t ORDER BY a LIMIT 2")
        check("ordered limit rows", rows == [["1"], ["2"]], f"{rows}")

        _, rows, _ = c.q("SELECT a FROM t ORDER BY a DESC LIMIT 2")
        check("order by desc limit nulls first", rows == [[None], ["3"]], f"{rows}")
        plan = plan_of(c, "SELECT a FROM t ORDER BY a DESC")
        check("order by desc uses index", "Index Scan using idx_a" in plan and "Sort" not in plan, plan)

        _, rows, _ = c.q("SELECT a FROM t ORDER BY a")
        check("nulls sort last ascending (index order)", rows == [["1"], ["2"], ["3"], [None]], f"{rows}")
        _, rows, _ = c.q("SELECT a FROM t ORDER BY a DESC")
        check("nulls sort first descending (index order)", rows == [[None], ["3"], ["2"], ["1"]], f"{rows}")

        plan = plan_of(c, "SELECT a FROM t WHERE a > 1 ORDER BY a")
        check("order by with where keeps sort node", "Sort" in plan, plan)
        _, rows, _ = c.q("SELECT a FROM t WHERE a > 1 ORDER BY a")
        check("order by with where rows", rows == [["2"], ["3"]], f"{rows}")

        plan = plan_of(c, "SELECT b FROM t ORDER BY b")
        check("order by non-indexed -> sort", "Sort" in plan, plan)

        # alias shadowing: ORDER BY must read the output expression, so no
        # index-order scan is allowed here.
        plan = plan_of(c, "SELECT a + 100 AS a FROM t ORDER BY a")
        check("aliased expr order by keeps sort", "Sort" in plan, plan)
        _, rows, _ = c.q("SELECT a + 100 AS a FROM t WHERE a IS NOT NULL ORDER BY a LIMIT 1")
        check("aliased expr order by value", rows == [["101"]], f"{rows}")

        # qualified order-by still uses the index
        plan = plan_of(c, "SELECT a FROM t ORDER BY t.a LIMIT 1")
        check("qualified order by uses index", "Index Scan using idx_a" in plan, plan)

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# E. EXPLAIN
# ---------------------------------------------------------------------------

def t_explain():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE a(id int, v int)")
        c.q("CREATE TABLE b(id int, w int)")
        c.q("INSERT INTO a VALUES (1,10),(2,20)")
        c.q("INSERT INTO b VALUES (1,100),(2,200)")
        c.q("CREATE INDEX idx_a_id ON a(id)")

        tags, rows, codes = c.q("EXPLAIN SELECT * FROM a WHERE id = 1")
        check("explain tag is EXPLAIN", tags == ["EXPLAIN"] and codes == [], f"{tags} {codes}")
        check("explain column is QUERY PLAN", c.last_columns == ["QUERY PLAN"], f"{c.last_columns}")
        plan = "\n".join(r[0] for r in rows)
        check("explain shows index cond", "Index Cond: (id = 1)" in plan, plan)
        check("explain shows rows estimate", "(rows=" in plan, plan)

        plan = plan_of(c, "SELECT * FROM a JOIN b ON a.id = b.id")
        check("explain join -> nested loop",
              "Nested Loop" in plan and "Seq Scan on a" in plan and "Seq Scan on b" in plan, plan)

        plan = plan_of(c, "SELECT count(*) FROM a")
        check("explain aggregate", "Aggregate" in plan, plan)

        plan = plan_of(c, "SELECT DISTINCT v FROM a")
        check("explain distinct -> unique", "Unique" in plan, plan)

        plan = plan_of(c, "SELECT v FROM a ORDER BY v LIMIT 1")
        check("explain sort+limit", "Sort" in plan and "Limit 1" in plan, plan)

        plan = plan_of(c, "SELECT * FROM (SELECT id FROM a) s WHERE id = 2")
        check("explain subquery scan", "Subquery Scan on s" in plan, plan)

        _, _, codes = c.q("EXPLAIN ANALYZE SELECT 1")
        check("explain analyze -> 0A000", codes == ["0A000"], f"{codes}")

        _, _, codes = c.q("EXPLAIN INSERT INTO a VALUES (3,30)")
        check("explain non-select -> error", codes != [], f"{codes}")

        # extended protocol: EXPLAIN completes with the EXPLAIN tag
        xc = XConn(srv.port)
        tags, rows, codes = xc.run_extended("EXPLAIN SELECT * FROM a WHERE id = 2")
        check("extended explain tag", tags == ["EXPLAIN"] and codes == [], f"{tags} {codes}")
        check("extended explain rows", any("Index Scan using idx_a_id" in r[0] for r in rows), f"{rows}")
        xc.close()

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# F. ANALYZE + pg_stats
# ---------------------------------------------------------------------------

def t_analyze():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE s(a int, b text)")
        c.q("INSERT INTO s VALUES (1,'x'),(1,'x'),(2,'y'),(NULL,'z'),(3,'x')")
        tags, _, codes = c.q("ANALYZE s")
        check("analyze tag", tags == ["ANALYZE"] and codes == [], f"{tags} {codes}")

        _, rows, codes = c.q("SELECT attname, null_frac, n_distinct FROM pg_stats WHERE tablename = 's' ORDER BY attname")
        check("pg_stats rows", codes == [] and [r[0] for r in rows] == ["a", "b"], f"{rows}")
        by_att = {r[0]: r for r in rows}
        check("null_frac for a", abs(float(by_att["a"][1]) - 0.2) < 1e-9, f"{by_att}")
        check("n_distinct for a", by_att["a"][2] == "3", f"{by_att}")
        check("n_distinct for b", by_att["b"][2] == "3", f"{by_att}")

        _, rows, _ = c.q("SELECT most_common_vals, most_common_freqs FROM pg_stats WHERE tablename='s' AND attname='a'")
        check("mcv lists 1 as most common",
              rows[0][0] == "{1,2,3}" and "0.4000" in rows[0][1], f"{rows}")

        # ANALYZE updates EXPLAIN estimates
        c.q("CREATE INDEX idx_s_a ON s(a)")
        plan = plan_of(c, "SELECT * FROM s WHERE a = 1")
        check("explain estimate after analyze", "(rows=2)" in plan, plan)

        _, _, codes = c.q("ANALYZE missing")
        check("analyze missing table -> 42P01", codes == ["42P01"], f"{codes}")

        tags, _, codes = c.q("ANALYZE")
        check("analyze all tag", tags == ["ANALYZE"] and codes == [], f"{tags} {codes}")

        # a real table named pg_stats takes precedence over the catalog view
        c.q("CREATE TABLE pg_stats(x int)")
        c.q("INSERT INTO pg_stats VALUES (7)")
        _, rows, codes = c.q("SELECT x FROM pg_stats")
        check("real pg_stats table wins", codes == [] and rows == [["7"]], f"{rows} {codes}")

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# G. Transactional DDL + MVCC
# ---------------------------------------------------------------------------

def t_txn_ddl_mvcc():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE t(a int)")
        c.q("INSERT INTO t VALUES (1),(2)")
        c.q("CREATE INDEX idx_a ON t(a)")

        # Transactional DDL on a private table (so the pre-existing idx_a
        # on t can't win planner tie-breaks). One txn drops the old
        # index and creates a new one; ROLLBACK must restore exactly the
        # old state.
        c.q("CREATE TABLE t2(a int)")
        c.q("INSERT INTO t2 VALUES (1),(2)")
        c.q("CREATE INDEX idx_t2 ON t2(a)")
        c.q("BEGIN")
        c.q("DROP INDEX idx_t2")
        c.q("CREATE INDEX idx_tmp ON t2(a)")
        plan = plan_of(c, "SELECT * FROM t2 WHERE a = 1")
        check("uncommitted index visible to own txn", "Index Scan using idx_tmp" in plan, plan)
        c.q("ROLLBACK")
        plan = plan_of(c, "SELECT * FROM t2 WHERE a = 1")
        check("rolled-back drop restores old index", "Index Scan using idx_t2" in plan, plan)
        _, _, codes = c.q("DROP INDEX idx_tmp")
        check("rolled-back create is gone", codes == ["42P01"], f"{codes}")
        c.q("DROP INDEX idx_t2")
        plan = plan_of(c, "SELECT * FROM t2 WHERE a = 1")
        check("committed drop stays gone", "Seq Scan on t2" in plan, plan)

        # own uncommitted row is found through the index, then rolled back
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (99)")
        _, rows, _ = c.q("SELECT a FROM t WHERE a = 99")
        check("own uncommitted row via index", rows == [["99"]], f"{rows}")
        c.q("ROLLBACK")
        _, rows, _ = c.q("SELECT a FROM t WHERE a = 99")
        check("rolled-back row gone from index", rows == [], f"{rows}")

        # Unique checks see only snapshot-visible rows: a concurrent
        # uncommitted insert does not conflict (no blocking in rustgres),
        # but a committed one does.
        c1, c2 = Conn(srv.port), Conn(srv.port)
        c1.q("CREATE UNIQUE INDEX uq_a ON t(a)")
        c1.q("BEGIN")
        c1.q("INSERT INTO t VALUES (100)")
        tags, _, codes = c2.q("INSERT INTO t VALUES (100)")
        check("concurrent uncommitted insert no conflict", tags == ["INSERT 0 1"] and codes == [], f"{tags} {codes}")
        c1.q("ROLLBACK")
        _, rows, _ = c2.q("SELECT count(*) FROM t WHERE a = 100")
        check("only the committed row remains", rows == [["1"]], f"{rows}")
        _, _, codes = c2.q("INSERT INTO t VALUES (100)")
        check("duplicate of committed row -> 23505", codes == ["23505"], f"{codes}")
        c2.q("DELETE FROM t WHERE a = 100")
        c1.close()
        c2.close()

        # an uncommitted index is invisible to other transactions
        c.q("CREATE TABLE t3(a int)")
        c.q("INSERT INTO t3 VALUES (1)")
        c1, c2 = Conn(srv.port), Conn(srv.port)
        c1.q("BEGIN")
        c1.q("CREATE INDEX idx_tmp2 ON t3(a)")
        plan = plan_of(c2, "SELECT * FROM t3 WHERE a = 1")
        check("uncommitted index invisible to others", "Seq Scan on t3" in plan, plan)
        c1.q("COMMIT")
        plan = plan_of(c2, "SELECT * FROM t3 WHERE a = 1")
        check("committed index visible afterwards", "Index Scan using idx_tmp2" in plan, plan)
        c1.close()
        c2.close()

        c.close()
    finally:
        srv.cleanup()


# ---------------------------------------------------------------------------
# H. Durability across kill -9
# ---------------------------------------------------------------------------

def t_durability():
    data_dir = tempfile.mkdtemp(prefix="rg8d_")
    port = alloc_port()
    try:
        srv = Server(port, data_dir)
        srv.start()
        c = Conn(port)
        c.q("CREATE TABLE t(a int, b text)")
        c.q("INSERT INTO t VALUES (1,'one'),(2,'two')")
        c.q("CREATE INDEX idx_a ON t(a)")
        c.q("CREATE UNIQUE INDEX uq_b ON t(b)")
        c.q("CHECKPOINT")
        c.close()
        srv.kill9()

        srv = Server(port, data_dir)
        srv.start()
        c = Conn(port)
        plan = plan_of(c, "SELECT * FROM t WHERE a = 2")
        check("index survives kill -9 (checkpoint)", "Index Scan using idx_a" in plan, plan)
        _, rows, _ = c.q("SELECT * FROM t WHERE a = 2 ORDER BY a")
        check("rows survive kill -9", rows == [["2", "two"]], f"{rows}")
        _, _, codes = c.q("INSERT INTO t VALUES (3,'two')")
        check("unique index enforced after recovery", codes == ["23505"], f"{codes}")
        c.q("DROP INDEX idx_a")
        c.close()
        srv.kill9()

        srv = Server(port, data_dir)
        srv.start()
        c = Conn(port)
        plan = plan_of(c, "SELECT * FROM t WHERE a = 1")
        check("dropped index stays dropped after recovery", "Seq Scan on t" in plan, plan)
        _, _, codes = c.q("DROP INDEX uq_b")
        check("unique index also survived", codes == [], f"{codes}")
        c.close()
        srv.stop()
    finally:
        shutil.rmtree(data_dir, ignore_errors=True)


# ---------------------------------------------------------------------------
# I. VACUUM interplay
# ---------------------------------------------------------------------------

def t_vacuum_index():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE t(a int)")
        c.q("INSERT INTO t VALUES (1),(2),(3)")
        c.q("CREATE INDEX idx_a ON t(a)")
        c.q("DELETE FROM t WHERE a = 2")
        c.q("VACUUM t")
        _, rows, _ = c.q("SELECT a FROM t WHERE a >= 1 ORDER BY a")
        check("index correct after vacuum", rows == [["1"], ["3"]], f"{rows}")
        plan = plan_of(c, "SELECT * FROM t WHERE a = 3")
        check("index still used after vacuum", "Index Scan using idx_a" in plan, plan)
        c.close()
    finally:
        srv.cleanup()


def main():
    t_ddl_basics()
    t_unique_enforcement()
    t_index_scans()
    t_order_by_index()
    t_explain()
    t_analyze()
    t_txn_ddl_mvcc()
    t_durability()
    t_vacuum_index()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
