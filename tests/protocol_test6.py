#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.6 (query engine).

Covers INNER/LEFT/CROSS joins with aliases and general ON predicates,
scalar/IN/EXISTS/derived-table subqueries (correlated), aggregates with
GROUP BY/HAVING, DISTINCT, OFFSET, ORDER BY extensions, SELECT ... FOR
UPDATE row locks (two-session conflicts, release on commit/rollback/
disconnect), NULL three-valued logic, and MVCC visibility of aggregates.

Unlike the v0.1-v0.3 suites this one manages its own server subprocesses:
each test boots a fresh server on a scratch data dir, so WAL replay from
earlier runs can never leak in.

Usage: build the server first (`cargo build`), then `python3 tests/protocol_test6.py`.
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

# Each test gets its own port: a lingering server from an earlier test can
# never steal another test's traffic (or its seed data).
_next_port = [55433]


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
        """Returns (tags, rows, err_codes)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
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
            elif t == b"Z":
                return tags, rows, codes

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        finally:
            self.s.close()

    def close_abrupt(self):
        # Raw socket close with no Terminate message: the server must notice
        # EOF on its next read and roll the open transaction back (and
        # release its row locks), exactly like a crashed client.
        self.s.close()


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
    def __init__(self, port):
        self.data_dir = tempfile.mkdtemp(prefix="rg6_")
        self.port = port
        self.proc = None

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
            # Don't hand the port to the next test until the old server is
            # really gone (defensive; tests use distinct ports anyway).
            wait_for_port_free(self.port)

    def cleanup(self):
        self.stop()
        shutil.rmtree(self.data_dir, ignore_errors=True)


def fresh_server():
    srv = Server(alloc_port())
    srv.start()
    return srv


def conns(srv, n=1):
    cs = [Conn(srv.port) for _ in range(n)]
    return cs[0] if n == 1 else cs


def seed(c):
    """users(id,name) x3 and orders(id,uid,amt) x3, committed."""
    c.q("CREATE TABLE users(id INT, name TEXT)")
    c.q("CREATE TABLE orders(id INT, uid INT, amt INT)")
    c.q("INSERT INTO users VALUES (1, 'ann'), (2, 'bob'), (3, 'cid')")
    c.q("INSERT INTO orders VALUES (101, 1, 10), (102, 1, 20), (103, 2, 5)")


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def t_inner_join():
    print("== INNER JOIN with qualified refs and ORDER BY ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, codes = c.q(
            "SELECT users.name, orders.amt FROM users "
            "JOIN orders ON users.id = orders.uid ORDER BY orders.amt"
        )
        check("inner join rows", rows == [["bob", "5"], ["ann", "10"], ["ann", "20"]], str(rows))
        check("no error", codes == [], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_join_aliases():
    print("== table aliases ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT u.name AS who, o.amt FROM users AS u "
            "INNER JOIN orders AS o ON u.id = o.uid ORDER BY who"
        )
        check("aliased join", rows == [["ann", "10"], ["ann", "20"], ["bob", "5"]], str(rows))
        # alias without AS
        _, rows, _ = c.q("SELECT u.name FROM users u WHERE u.id = 2")
        check("bare alias", rows == [["bob"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_left_join():
    print("== LEFT JOIN keeps unmatched rows with NULLs ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT u.name, o.amt FROM users u LEFT JOIN orders o "
            "ON u.id = o.uid ORDER BY u.id, o.amt"
        )
        check("left join rows", rows == [
            ["ann", "10"], ["ann", "20"], ["bob", "5"], ["cid", None],
        ], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_join_general_on():
    print("== general ON predicates (non-equality, AND) ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        # inequality join: every user paired with orders of strictly greater uid
        _, rows, _ = c.q(
            "SELECT u.name, o.id FROM users u JOIN orders o ON u.id < o.uid "
            "ORDER BY u.name, o.id"
        )
        check("inequality ON", rows == [
            ["ann", "103"],
        ], str(rows))
        # AND in ON
        _, rows, _ = c.q(
            "SELECT u.name FROM users u JOIN orders o "
            "ON u.id = o.uid AND o.amt > 15 ORDER BY u.name"
        )
        check("AND in ON", rows == [["ann"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_cross_join():
    print("== comma FROM is a CROSS JOIN ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q("SELECT count(*) FROM users, orders")
        check("3x3 cross join", rows == [["9"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_right_full_join():
    print("== RIGHT/FULL JOIN (v0.20.1: native, no swap) ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        c.q("CREATE TABLE jl(id INT, v TEXT)")
        c.q("CREATE TABLE jr(id INT, w TEXT)")
        c.q("INSERT INTO jl VALUES (1, 'a1'), (2, 'a2')")
        c.q("INSERT INTO jr VALUES (2, 'b2'), (3, 'b3')")
        # RIGHT keeps unmatched right rows; LEFT unmatched rows vanish.
        _, rows, codes = c.q(
            "SELECT jl.id, jr.id FROM jl RIGHT JOIN jr ON jl.id = jr.id ORDER BY 2"
        )
        check("right join rows", rows == [["2", "2"], [None, "3"]], str(rows))
        check("right join no error", codes == [], str(codes))
        # FULL keeps both sides.
        _, rows, _ = c.q(
            "SELECT jl.id, jr.id FROM jl FULL JOIN jr ON jl.id = jr.id ORDER BY 1, 2"
        )
        check("full join rows", rows == [["1", None], ["2", "2"], [None, "3"]], str(rows))
        # v0.20.1 parser regression: RIGHT/FULL must not be eaten as table aliases.
        _, rows, codes = c.q(
            "SELECT jl.id, jr.id FROM jl RIGHT JOIN jr ON jl.id = jr.id"
        )
        check("right not parsed as alias", codes == [], str(codes))
        # Aliased RIGHT JOIN.
        _, rows, _ = c.q(
            "SELECT x.id, y.id FROM jl AS x RIGHT JOIN jr AS y ON x.id = y.id ORDER BY 2"
        )
        check("aliased right join", rows == [["2", "2"], [None, "3"]], str(rows))
        # Empty left input: RIGHT JOIN still yields unmatched right rows.
        c.q("CREATE TABLE je(id INT)")
        _, rows, _ = c.q("SELECT je.id, jr.id FROM je RIGHT JOIN jr ON je.id = jr.id ORDER BY 2")
        check("right join empty left", rows == [[None, "2"], [None, "3"]], str(rows))
        # WHERE on the preserved side stays sound with pushdown.
        _, rows, _ = c.q(
            "SELECT jl.id, jr.id FROM jl RIGHT JOIN jr ON jl.id = jr.id WHERE jr.id = 3"
        )
        check("right join where pushdown", rows == [[None, "3"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_join_errors():
    print("== join resolution errors ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, _, codes = c.q("SELECT id FROM users JOIN orders ON users.id = orders.uid")
        check("ambiguous column is 42702", codes == ["42702"], str(codes))
        _, _, codes = c.q("SELECT nope FROM users")
        check("unknown column is 42703", codes == ["42703"], str(codes))
        _, _, codes = c.q("SELECT u.nope FROM users u")
        check("unknown qualified column is 42703", codes == ["42703"], str(codes))
        _, _, codes = c.q("SELECT * FROM missing")
        check("unknown table is 42P01", codes == ["42P01"], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_scalar_subquery():
    print("== scalar subqueries (correlated) ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT name, (SELECT count(*) FROM orders o WHERE o.uid = users.id) "
            "AS n FROM users ORDER BY id"
        )
        check("correlated scalar subquery", rows == [
            ["ann", "2"], ["bob", "1"], ["cid", "0"],
        ], str(rows))
        # uncorrelated: empty result -> NULL
        _, rows, _ = c.q("SELECT (SELECT name FROM users WHERE id = 99) IS NULL AS e")
        check("empty scalar subquery is NULL", rows == [["t"]], str(rows))
        # too many rows -> 21000
        _, _, codes = c.q("SELECT (SELECT id FROM users)")
        check("multi-row scalar subquery is 21000", codes == ["21000"], str(codes))
        # too many columns -> 42601
        _, _, codes = c.q("SELECT (SELECT id, name FROM users WHERE id = 1)")
        check("multi-column scalar subquery is 42601", codes == ["42601"], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_in_subquery():
    print("== IN / NOT IN subqueries with NULL semantics ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT name FROM users WHERE id IN (SELECT uid FROM orders) ORDER BY id"
        )
        check("IN subquery", rows == [["ann"], ["bob"]], str(rows))
        _, rows, _ = c.q(
            "SELECT name FROM users WHERE id NOT IN (SELECT uid FROM orders) ORDER BY id"
        )
        check("NOT IN subquery", rows == [["cid"]], str(rows))
        # a NULL in the subquery result makes NOT IN return no rows (SQL semantics)
        c.q("INSERT INTO orders VALUES (104, NULL, 1)")
        _, rows, _ = c.q("SELECT name FROM users WHERE id NOT IN (SELECT uid FROM orders)")
        check("NOT IN with NULL yields no rows", rows == [], str(rows))
        # but IN still finds matches
        _, rows, _ = c.q(
            "SELECT name FROM users WHERE id IN (SELECT uid FROM orders) ORDER BY id"
        )
        check("IN unaffected by NULL", rows == [["ann"], ["bob"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_exists():
    print("== EXISTS / NOT EXISTS (correlated) ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT name FROM users u WHERE EXISTS "
            "(SELECT 1 FROM orders o WHERE o.uid = u.id AND o.amt > 15)"
        )
        check("EXISTS", rows == [["ann"]], str(rows))
        _, rows, _ = c.q(
            "SELECT name FROM users u WHERE NOT EXISTS "
            "(SELECT 1 FROM orders o WHERE o.uid = u.id) ORDER BY u.id"
        )
        check("NOT EXISTS", rows == [["cid"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_derived_table():
    print("== derived tables in FROM ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT t.uid, t.total FROM "
            "(SELECT uid, sum(amt) AS total FROM orders GROUP BY uid) AS t "
            "WHERE t.total > 10 ORDER BY t.uid"
        )
        check("derived table", rows == [["1", "30"]], str(rows))
        # v0.14: alias-less derived tables are allowed (auto-named
        # `unnamed_subquery`); PostgreSQL requires an alias, but the
        # pg_regress conformance corpus uses them, so rustgres is
        # intentionally more permissive here.
        _, rows, codes = c.q("SELECT * FROM (SELECT 1)")
        check("derived table without alias works", not codes, str(codes))
        # derived table columns are addressable by the alias
        _, rows, _ = c.q("SELECT s.a FROM (SELECT 1 AS a, 2 AS b) s")
        check("derived table projection", rows == [["1"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_aggregates():
    print("== aggregates: COUNT/SUM/AVG/MIN/MAX ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q("SELECT count(*), count(uid), sum(amt), avg(amt), min(amt), max(amt) FROM orders")
        check("global aggregates", rows == [["3", "3", "35", "11.666666666666666", "5", "20"]], str(rows))
        # count(col) skips NULLs; sum of no rows is NULL
        c.q("INSERT INTO orders VALUES (105, NULL, NULL)")
        _, rows, _ = c.q("SELECT count(*), count(amt), sum(amt) FROM orders")
        check("NULLs skipped", rows == [["4", "3", "35"]], str(rows))
        _, rows, _ = c.q("SELECT sum(amt) FROM orders WHERE id > 1000")
        check("sum of no rows is NULL", rows == [[None]], str(rows))
        _, rows, _ = c.q("SELECT count(*) FROM orders WHERE id > 1000")
        check("count of no rows is 0", rows == [["0"]], str(rows))
        # avg returns float even for ints
        _, rows, _ = c.q("SELECT avg(amt) FROM orders WHERE uid = 1")
        check("avg is float", rows == [["15"]], str(rows))
        # type errors
        _, _, codes = c.q("SELECT sum(name) FROM users")
        check("sum(text) is 42883", codes == ["42883"], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_group_by_having():
    print("== multi-column GROUP BY + HAVING ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q(
            "SELECT uid, count(*), sum(amt) FROM orders GROUP BY uid ORDER BY uid"
        )
        check("group by", rows == [["1", "2", "30"], ["2", "1", "5"]], str(rows))
        _, rows, _ = c.q(
            "SELECT uid, amt, count(*) FROM orders GROUP BY uid, amt ORDER BY uid, amt"
        )
        check("multi-column group by", rows == [
            ["1", "10", "1"], ["1", "20", "1"], ["2", "5", "1"],
        ], str(rows))
        _, rows, _ = c.q(
            "SELECT uid, count(*) AS n FROM orders GROUP BY uid HAVING count(*) > 1"
        )
        check("having", rows == [["1", "2"]], str(rows))
        # bare non-grouped column -> 42803
        _, _, codes = c.q("SELECT uid, amt FROM orders GROUP BY uid")
        check("bare column not in GROUP BY is 42803", codes == ["42803"], str(codes))
        # aggregates not allowed in WHERE -> 42803
        _, _, codes = c.q("SELECT uid FROM orders WHERE count(*) > 1 GROUP BY uid")
        check("aggregate in WHERE is 42803", codes == ["42803"], str(codes))
        # but aggregates ARE allowed in HAVING and ORDER BY
        _, rows, _ = c.q(
            "SELECT uid FROM orders GROUP BY uid HAVING sum(amt) > 10 ORDER BY sum(amt) DESC"
        )
        check("agg in having/order by", rows == [["1"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_distinct_offset():
    print("== DISTINCT and OFFSET ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q("SELECT DISTINCT uid FROM orders ORDER BY uid")
        check("distinct", rows == [["1"], ["2"]], str(rows))
        _, rows, _ = c.q("SELECT id FROM orders ORDER BY id LIMIT 2 OFFSET 1")
        check("limit+offset", rows == [["102"], ["103"]], str(rows))
        _, rows, _ = c.q("SELECT id FROM orders ORDER BY id OFFSET 2")
        check("offset alone", rows == [["103"]], str(rows))
        _, rows, _ = c.q("SELECT id FROM orders ORDER BY id OFFSET 99")
        check("offset past the end", rows == [], str(rows))
        _, _, codes = c.q("SELECT id FROM orders OFFSET -1")
        check("negative offset errors", codes != [], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_order_by_forms():
    print("== ORDER BY position, alias, non-projected column ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, _ = c.q("SELECT name, amt FROM users JOIN orders ON users.id = orders.uid ORDER BY 2 DESC")
        check("order by position", rows == [["ann", "20"], ["ann", "10"], ["bob", "5"]], str(rows))
        _, rows, _ = c.q("SELECT name AS n FROM users ORDER BY n DESC")
        check("order by alias", rows == [["cid"], ["bob"], ["ann"]], str(rows))
        # v0.1-v0.5 behavior kept: ORDER BY may name a non-projected column
        _, rows, _ = c.q("SELECT name FROM users ORDER BY id DESC")
        check("order by non-projected column", rows == [["cid"], ["bob"], ["ann"]], str(rows))
        _, _, codes = c.q("SELECT name FROM users ORDER BY 5")
        check("bad order-by position errors", codes == ["42601"], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_null_semantics():
    print("== NULL three-valued logic ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        c.q("INSERT INTO users VALUES (4, NULL)")
        _, rows, _ = c.q("SELECT id FROM users WHERE name = NULL ORDER BY id")
        check("= NULL matches nothing", rows == [], str(rows))
        _, rows, _ = c.q("SELECT id FROM users WHERE name IS NULL")
        check("IS NULL", rows == [["4"]], str(rows))
        _, rows, _ = c.q("SELECT id FROM users WHERE name IS NOT NULL ORDER BY id")
        check("IS NOT NULL", rows == [["1"], ["2"], ["3"]], str(rows))
        # NULLS LAST for ASC (Postgres default)
        _, rows, _ = c.q("SELECT id FROM users ORDER BY name")
        check("nulls last on ASC", rows[0] == ["1"] and rows[-1] == ["4"], str(rows))
        # AND/OR with NULLs
        _, rows, _ = c.q("SELECT id FROM users WHERE id = 1 AND NULL")
        check("TRUE AND NULL is not true", rows == [], str(rows))
        _, rows, _ = c.q("SELECT id FROM users WHERE id = 1 OR NULL")
        check("TRUE OR NULL is true", rows == [["1"]], str(rows))
        _, rows, _ = c.q("SELECT NOT NULL IS NULL AS x")
        check("NOT NULL IS NULL", rows == [["f"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_for_update_conflict():
    print("== SELECT FOR UPDATE: two-session lock conflict ==")
    srv = fresh_server()
    try:
        c1, c2 = conns(srv, 2)
        seed(c1)
        c1.q("BEGIN")
        _, rows, codes = c1.q("SELECT * FROM users WHERE id = 1 FOR UPDATE")
        check("locking read returns the row", rows == [["1", "ann"]] and codes == [], str((rows, codes)))
        # another session writing the locked row fails fast with 40001
        # (never waits). The failed statement aborts c2's transaction, so
        # roll back before the next probe.
        c2.q("BEGIN")
        _, _, codes = c2.q("UPDATE users SET name = 'x' WHERE id = 1")
        check("locked row update is 40001", codes == ["40001"], str(codes))
        c2.q("ROLLBACK")
        c2.q("BEGIN")
        _, _, codes = c2.q("DELETE FROM users WHERE id = 1")
        check("locked row delete is 40001", codes == ["40001"], str(codes))
        c2.q("ROLLBACK")
        # ...but a different row is fine
        c2.q("BEGIN")
        tags, _, codes = c2.q("UPDATE users SET name = 'b2' WHERE id = 2")
        check("unlocked row update works", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c2.q("COMMIT")
        # locking read of the same row by the other session also fails fast
        c2.q("BEGIN")
        _, _, codes = c2.q("SELECT * FROM users WHERE id = 1 FOR UPDATE")
        check("second FOR UPDATE is 40001", codes == ["40001"], str(codes))
        c2.q("ROLLBACK")
        c1.q("COMMIT")  # releases the lock
        c2.q("BEGIN")
        tags, _, codes = c2.q("UPDATE users SET name = 'x' WHERE id = 1")
        check("lock released on commit", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c2.q("COMMIT")
        _, rows, _ = c1.q("SELECT name FROM users WHERE id = 1")
        check("committed write visible", rows == [["x"]], str(rows))
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_for_update_rollback_releases():
    print("== SELECT FOR UPDATE: lock released on ROLLBACK ==")
    srv = fresh_server()
    try:
        c1, c2 = conns(srv, 2)
        seed(c1)
        c1.q("BEGIN")
        c1.q("SELECT * FROM users WHERE id = 1 FOR UPDATE")
        c1.q("ROLLBACK")
        tags, _, codes = c2.q("UPDATE users SET name = 'y' WHERE id = 1")
        check("lock released on rollback", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_for_update_disconnect_releases():
    print("== SELECT FOR UPDATE: lock released on disconnect ==")
    srv = fresh_server()
    try:
        c1, c2 = conns(srv, 2)
        seed(c1)
        c1.q("BEGIN")
        c1.q("SELECT * FROM users WHERE id = 2 FOR UPDATE")
        c1.close_abrupt()  # crashed client: txn aborts, locks release
        time.sleep(0.5)
        tags, _, codes = c2.q("UPDATE users SET name = 'z' WHERE id = 2")
        check("lock released on disconnect", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c2.close()
    finally:
        srv.cleanup()


def t_for_update_autocommit():
    print("== SELECT FOR UPDATE outside a txn takes and releases cleanly ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, rows, codes = c.q("SELECT * FROM users WHERE id = 1 FOR UPDATE")
        check("autocommit FOR UPDATE works", rows == [["1", "ann"]] and codes == [], str((rows, codes)))
        # lock did not leak: a new transaction can write the row
        c.q("BEGIN")
        tags, _, codes = c.q("UPDATE users SET name = 'w' WHERE id = 1")
        check("no leaked lock", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c.q("COMMIT")
        c.close()
    finally:
        srv.cleanup()


def t_for_update_restrictions():
    print("== FOR UPDATE restrictions ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        _, _, codes = c.q("SELECT DISTINCT uid FROM orders FOR UPDATE")
        check("FOR UPDATE with DISTINCT is 0A000", codes == ["0A000"], str(codes))
        _, _, codes = c.q("SELECT uid, count(*) FROM orders GROUP BY uid FOR UPDATE")
        check("FOR UPDATE with GROUP BY is 0A000", codes == ["0A000"], str(codes))
        _, _, codes = c.q("SELECT count(*) FROM orders FOR UPDATE")
        check("FOR UPDATE with aggregate is 0A000", codes == ["0A000"], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_mvcc_aggregates():
    print("== aggregates respect MVCC visibility ==")
    srv = fresh_server()
    try:
        c1, c2 = conns(srv, 2)
        seed(c1)
        c1.q("BEGIN")
        c1.q("INSERT INTO orders VALUES (200, 3, 100)")
        _, rows, _ = c2.q("SELECT count(*), sum(amt) FROM orders")
        check("uncommitted row invisible to aggregate", rows == [["3", "35"]], str(rows))
        _, rows, _ = c1.q("SELECT count(*), sum(amt) FROM orders")
        check("own write visible to aggregate", rows == [["4", "135"]], str(rows))
        c1.q("COMMIT")
        _, rows, _ = c2.q("SELECT count(*), sum(amt) FROM orders")
        check("visible after commit", rows == [["4", "135"]], str(rows))
        # joined aggregate across an update
        c1.q("BEGIN")
        c1.q("UPDATE orders SET amt = 1000 WHERE id = 101")
        _, rows, _ = c2.q(
            "SELECT u.name, sum(o.amt) FROM users u JOIN orders o ON u.id = o.uid "
            "GROUP BY u.name ORDER BY u.name"
        )
        check("join+aggregate sees old version", rows == [
            ["ann", "30"], ["bob", "5"], ["cid", "100"],
        ], str(rows))
        c1.q("COMMIT")
        _, rows, _ = c2.q(
            "SELECT u.name, sum(o.amt) FROM users u JOIN orders o ON u.id = o.uid "
            "GROUP BY u.name ORDER BY u.name"
        )
        check("join+aggregate sees new version", rows == [
            ["ann", "1020"], ["bob", "5"], ["cid", "100"],
        ], str(rows))
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_extended_join_params():
    print("== extended protocol: params in JOIN ... ON ==")
    srv = fresh_server()
    try:
        c = conns(srv)
        seed(c)
        # raw extended-protocol exchange: Parse/Bind/Execute with one param
        s = c.s
        def xmsg(typ, payload):
            return typ + struct.pack("!i", len(payload) + 4) + payload
        s.sendall(xmsg(b"P", b"\x00"
                      + cstr("SELECT u.name FROM users u JOIN orders o ON u.id = o.uid AND o.amt > $1")
                      + struct.pack("!h", 0)))
        s.sendall(xmsg(b"B", b"\x00\x00" + struct.pack("!h", 0) + struct.pack("!h", 1)
                      + struct.pack("!i", 2) + b"15" + struct.pack("!h", 0)))
        s.sendall(xmsg(b"D", b"P\x00"))
        s.sendall(xmsg(b"E", b"\x00" + struct.pack("!i", 0)))
        s.sendall(xmsg(b"S", b""))
        rows = []
        while True:
            t = s.recv(1)
            (ln,) = struct.unpack("!i", c._read_exact(4))
            p = c._read_exact(ln - 4)
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln2,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    r.append(p[pos:pos + ln2].decode())
                    pos += ln2
                rows.append(r)
            elif t == b"Z":
                break
            elif t == b"E":
                raise AssertionError(f"unexpected error: {p!r}")
        check("param in JOIN ON", rows == [["ann"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def t_for_update_join_locks():
    print("== SELECT FOR UPDATE over a JOIN locks rows from every table ==")
    srv = fresh_server()
    try:
        c1, c2 = conns(srv, 2)
        seed(c1)
        c1.q("BEGIN")
        _, rows, codes = c1.q(
            "SELECT u.name, o.id FROM users u JOIN orders o ON u.id = o.uid "
            "WHERE u.id = 1 FOR UPDATE")
        check("join locking read returns 2 rows",
              rows == [["ann", "101"], ["ann", "102"]] and codes == [], str((rows, codes)))
        # the user row and both order rows are locked; an unrelated order is not
        c2.q("BEGIN")
        _, _, codes = c2.q("UPDATE users SET name = 'x' WHERE id = 1")
        check("locked user row is 40001", codes == ["40001"], str(codes))
        c2.q("ROLLBACK")
        c2.q("BEGIN")
        _, _, codes = c2.q("UPDATE orders SET amt = 1 WHERE id = 101")
        check("locked order row is 40001", codes == ["40001"], str(codes))
        c2.q("ROLLBACK")
        c2.q("BEGIN")
        tags, _, codes = c2.q("UPDATE orders SET amt = 1 WHERE id = 103")
        check("unrelated order row writable", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c2.q("COMMIT")
        c1.q("COMMIT")
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_savepoint_releases_locks():
    print("== ROLLBACK TO SAVEPOINT releases locks taken after it ==")
    srv = fresh_server()
    try:
        c1, c2 = conns(srv, 2)
        seed(c1)
        c1.q("BEGIN")
        c1.q("SELECT * FROM users WHERE id = 1 FOR UPDATE")
        c1.q("SAVEPOINT s")
        c1.q("SELECT * FROM users WHERE id = 2 FOR UPDATE")
        c1.q("ROLLBACK TO SAVEPOINT s")
        # id=1 was locked before the savepoint: still locked
        c2.q("BEGIN")
        _, _, codes = c2.q("UPDATE users SET name = 'x' WHERE id = 1")
        check("pre-savepoint lock survives", codes == ["40001"], str(codes))
        c2.q("ROLLBACK")
        # id=2 was locked after the savepoint: released
        c2.q("BEGIN")
        tags, _, codes = c2.q("UPDATE users SET name = 'y' WHERE id = 2")
        check("post-savepoint lock released", tags == ["UPDATE 1"] and codes == [], str((tags, codes)))
        c2.q("COMMIT")
        c1.q("COMMIT")
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def main():
    if not os.path.exists(BIN):
        print(f"build the server first: {BIN} missing")
        sys.exit(2)
    t_inner_join()
    t_join_aliases()
    t_left_join()
    t_right_full_join()
    t_join_general_on()
    t_cross_join()
    t_join_errors()
    t_scalar_subquery()
    t_in_subquery()
    t_exists()
    t_derived_table()
    t_aggregates()
    t_group_by_having()
    t_distinct_offset()
    t_order_by_forms()
    t_null_semantics()
    t_for_update_conflict()
    t_for_update_join_locks()
    t_savepoint_releases_locks()
    t_for_update_rollback_releases()
    t_for_update_disconnect_releases()
    t_for_update_autocommit()
    t_for_update_restrictions()
    t_mvcc_aggregates()
    t_extended_join_params()
    print()
    print(f"{len(passed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
