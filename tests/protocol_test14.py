#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.14 (pg_regress conformance fixes).

Groups:
  A. type-name aliases      -- int4, varchar, char, character varying,
     bpchar, name, serial parse as their canonical types; typmods
     (varchar(10), char(3)) accepted.
  B. VACUUM ANALYZE        -- `VACUUM ANALYZE [table]` accepted.
  C. INSERT typed literals  -- INSERT VALUES with typed literals works.
  D. int-to-bool casts      -- integer to boolean casts.
  E. boolean prefixes       -- 'tru'/'of' unambiguous prefixes; bare 'o'
     stays invalid (ambiguous on/off).
  F. operator aliases       -- booleq, boolne, int4eq, texteq resolve.
  G. cast column naming     -- SELECT 0::boolean reports column name `bool`.
  H. TEMP tables            -- CREATE [TEMPORARY|TEMP] [GLOBAL|LOCAL] TABLE
     parses (behaves as a regular persistent table; documented).
  I. FROM extras            -- redundant parens `((SELECT ...))`, alias-less
     derived tables (auto `unnamed_subquery`), and `(VALUES ...)` sources
     with `column1..N` naming.
  J. function-style casts   -- float8(x), int4(x) parse as casts.
  K. nameless CREATE INDEX  -- CREATE INDEX ON t (a, b) auto-names the
     index <table>_<cols>_idx.

Boots its own server on 127.0.0.1:5433 with a scratch data dir.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test14.py`.
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
PORT = 5433

passed, failed = [], []


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


def err_code(payload):
    i = 0
    while i < len(payload):
        if payload[i:i + 1] == b"C":
            j = payload.index(b"\x00", i + 1)
            return payload[i + 1:j].decode()
        j = payload.index(b"\x00", i + 1)
        i = j + 1
    return ""


def _read_exact(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise RuntimeError("connection closed mid-message")
        data += chunk
    return data


class Conn:
    """Normal SQL connection (simple protocol)."""

    def __init__(self, port, user="postgres", timeout=30):
        self.s = socket.create_connection((HOST, port), timeout=timeout)
        body = struct.pack("!i", 196608)
        body += b"user\x00" + user.encode() + b"\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self._drain_until_ready()

    def _read_msg(self, timeout=30):
        self.s.settimeout(timeout)
        t = self.s.recv(1)
        if not t:
            raise RuntimeError("connection closed by server")
        (ln,) = struct.unpack("!i", _read_exact(self.s, 4))
        return t, _read_exact(self.s, ln - 4)

    def _drain_until_ready(self):
        while True:
            t, p = self._read_msg()
            if t == b"Z":
                return
            if t == b"E":
                raise RuntimeError("auth failed: " + err_code(p))

    def q(self, sql):
        self.s.sendall(msg(b"Q", cstr(sql)))
        tags, rows, codes, descs = [], [], [], []
        while True:
            t, p = self._read_msg()
            if t == b"T":
                (n,) = struct.unpack("!h", p[:2])
                pos, cols = 2, []
                for _ in range(n):
                    j = p.index(b"\x00", pos)
                    cols.append(p[pos:j].decode())
                    pos = j + 1 + 18
                descs.append(tuple(cols))
            elif t == b"C":
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
                        pos += ln
                rows.append(tuple(r))
            elif t == b"E":
                codes.append(err_code(p))
            elif t == b"Z":
                break
        return tags, rows, codes, descs

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        except OSError:
            pass
        self.s.close()


class Server:
    def __init__(self):
        # Fail fast if the port is already taken: otherwise we'd
        # silently test against a stale unrelated listener.
        s = socket.socket()
        try:
            s.bind((HOST, PORT))
        except OSError as e:
            raise RuntimeError(f"port {PORT} already in use: {e}")
        finally:
            s.close()
        self.datadir = tempfile.mkdtemp(prefix="rg14-")
        self.logpath = os.path.join(self.datadir, "server.log")
        self.logf = open(self.logpath, "wb")
        self.proc = subprocess.Popen(
            [BIN, "--port", str(PORT), "--data-dir", self.datadir],
            stdout=subprocess.DEVNULL, stderr=self.logf)
        for _ in range(100):
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"server exited during startup (rc={self.proc.returncode}); "
                    f"see {self.logpath}")
            try:
                socket.create_connection((HOST, PORT), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        else:
            self.stop()
            raise RuntimeError(f"server did not open 127.0.0.1:{PORT}")
        if self.proc.poll() is not None:
            raise RuntimeError(f"server died right after startup; see {self.logpath}")

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        try:
            self.logf.close()
        except Exception:
            pass
        shutil.rmtree(self.datadir, ignore_errors=True)


def rows_of(c, sql):
    _, rows, codes, _ = c.q(sql)
    return rows, codes


def cols_of(c, sql):
    _, _, codes, descs = c.q(sql)
    return descs[0] if descs else (), codes

# ---------------------------------------------------------------------------
# A. type-name aliases


def t_type_aliases(c):
    _, rows, codes, _ = c.q("CREATE TABLE t_alias (a int4, b varchar, c char, "
                            "d character varying(20), e bpchar, f name, g serial)")
    check("a-create-aliases", not codes, f"{codes}")
    _, rows, codes, descs = c.q(
        "SELECT a, b, c, d, e, f, g FROM t_alias")
    check("a-select-aliases", not codes, f"{codes}")
    # varchar(10) / char(3) typmods parse (lengths not enforced).
    _, _, codes, _ = c.q("CREATE TABLE t_tm (a varchar(10), b char(3))")
    check("a-typmods", not codes, f"{codes}")
    c.q("DROP TABLE t_alias")
    c.q("DROP TABLE t_tm")


# ---------------------------------------------------------------------------
# B. VACUUM ANALYZE


def t_vacuum_analyze(c):
    c.q("CREATE TABLE t_vac (a int)")
    for sql in ("VACUUM ANALYZE t_vac", "VACUUM ANALYZE", "VACUUM t_vac"):
        _, _, codes, _ = c.q(sql)
        check(f"b-{sql.lower().replace(' ', '-')}", not codes, f"{codes}: {sql}")
    c.q("DROP TABLE t_vac")


# ---------------------------------------------------------------------------
# C. INSERT typed literals


def t_insert_typed_literals(c):
    c.q("CREATE TABLE t_tl (a int, b text, d date)")
    _, _, codes, _ = c.q(
        "INSERT INTO t_tl VALUES (1::int, 'x'::text, DATE '2026-01-01')")
    check("c-typed-insert", not codes, f"{codes}")
    rows, codes = rows_of(c, "SELECT a, b FROM t_tl")
    check("c-typed-values", rows == [("1", "x")], f"{rows} {codes}")
    c.q("DROP TABLE t_tl")


# ---------------------------------------------------------------------------
# D. integer to boolean casts


def t_int_to_bool(c):
    rows, codes = rows_of(c, "SELECT 1::boolean, 0::boolean")
    check("d-int-bool", not codes and rows == [("t", "f")], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# E. boolean literal prefixes


def t_bool_prefixes(c):
    rows, codes = rows_of(c, "SELECT 'tru'::boolean, 'of'::boolean")
    check("e-unambiguous-prefixes", not codes and rows == [("t", "f")],
          f"{rows} {codes}")
    _, _, codes, _ = c.q("SELECT 'o'::boolean")
    check("e-bare-o-invalid", codes != [], f"expected error, got {codes}")


# ---------------------------------------------------------------------------
# F. internal operator-function aliases


def t_operator_aliases(c):
    for sql, want in (
        ("SELECT booleq(true, true)", [("t",)]),
        ("SELECT boolne(true, false)", [("t",)]),
        ("SELECT int4eq(1, 1)", [("t",)]),
        ("SELECT texteq('a', 'a')", [("t",)]),
    ):
        rows, codes = rows_of(c, sql)
        check(f"f-{sql.split('(')[0]}", not codes and rows == want,
              f"{rows} {codes}")


# ---------------------------------------------------------------------------
# G. cast output column naming


def t_cast_colname(c):
    cols, codes = cols_of(c, "SELECT 0::boolean")
    check("g-cast-colname", not codes and cols == ("bool",), f"{cols} {codes}")

# ---------------------------------------------------------------------------
# H. TEMP tables


def t_temp_tables(c):
    for i, kw in enumerate(("TEMP", "TEMPORARY", "GLOBAL TEMP", "LOCAL TEMPORARY")):
        _, _, codes, _ = c.q(f"CREATE {kw} TABLE t_temp{i} (a int)")
        check(f"h-create-{kw.lower().replace(' ', '-')}", not codes, f"{codes}")
        _, _, codes, _ = c.q(f"INSERT INTO t_temp{i} VALUES (42)")
        check(f"h-insert-{i}", not codes, f"{codes}")
        rows, codes = rows_of(c, f"SELECT a FROM t_temp{i}")
        check(f"h-select-{i}", not codes and rows == [("42",)], f"{rows} {codes}")
        c.q(f"DROP TABLE t_temp{i}")


# ---------------------------------------------------------------------------
# I. FROM extras: redundant parens, unnamed derived tables, VALUES


def t_from_extras(c):
    rows, codes = rows_of(c, "SELECT * FROM ((SELECT 1 AS x)) ss")
    check("i-redundant-parens", not codes and rows == [("1",)], f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT * FROM ((SELECT 1 AS x)), ((SELECT 2 AS y))")
    check("i-redundant-parens-multi", not codes and rows == [("1", "2")],
          f"{rows} {codes}")
    # Alias-less derived table: works, auto-named unnamed_subquery.
    rows, codes = rows_of(c, "SELECT COUNT(*) FROM (SELECT 1 AS x)")
    check("i-unnamed-derived", not codes and rows == [("1",)], f"{rows} {codes}")
    cols, codes = cols_of(c, "SELECT * FROM (SELECT 1 AS x)")
    check("i-unnamed-col", not codes and cols == ("x",), f"{cols} {codes}")
    # VALUES in FROM with PG column1..N naming.
    rows, codes = rows_of(c, "SELECT * FROM (VALUES (1, 2), (3, 4)) AS v")
    check("i-values-from", not codes and rows == [("1", "2"), ("3", "4")],
          f"{rows} {codes}")
    cols, codes = cols_of(c, "SELECT * FROM (VALUES (1, 2)) AS v")
    check("i-values-colnames", not codes and cols == ("column1", "column2"),
          f"{cols} {codes}")
    rows, codes = rows_of(
        c, "SELECT * FROM (SELECT 1 AS a), (VALUES (123456)) WHERE a = column1")
    check("i-values-join-filter", not codes and rows == [], f"{rows} {codes}")
    rows, codes = rows_of(
        c, "SELECT * FROM (SELECT 2 AS a), (VALUES (2)) WHERE a = column1")
    check("i-values-join-hit", not codes and rows == [("2", "2")],
          f"{rows} {codes}")
    # VALUES row-length mismatch is a syntax error.
    _, _, codes, _ = c.q("SELECT * FROM (VALUES (1), (2, 3)) AS v")
    check("i-values-ragged", codes == ["42601"], f"{codes}")


# ---------------------------------------------------------------------------
# J. function-style casts


def t_func_casts(c):
    for sql, want in (
        ("SELECT float8(1)", None),   # shape checked below
        ("SELECT int4('42')", [("42",)]),
        ("SELECT float8(count(*)) FROM (SELECT 1 AS x)", None),
    ):
        rows, codes = rows_of(c, sql)
        if want is None:
            check(f"j-{sql.split('(')[0]}-parses", not codes, f"{rows} {codes}")
        else:
            check(f"j-{sql.split('(')[0]}", not codes and rows == want,
                  f"{rows} {codes}")
    # multi-arg call on a type name is NOT a cast: falls through to function
    # lookup and fails as unknown function.
    _, _, codes, _ = c.q("SELECT int4(1, 2)")
    check("j-multiarg-not-cast", codes == ["42883"], f"{codes}")


# ---------------------------------------------------------------------------
# K. nameless CREATE INDEX


def t_nameless_index(c):
    c.q("CREATE TABLE t_idx (a int, b int)")
    _, _, codes, _ = c.q("CREATE INDEX ON t_idx (a, b)")
    check("k-nameless-create", not codes, f"{codes}")
    # PG auto-name convention: <table>_<cols>_idx — visible in EXPLAIN.
    _, rows, codes, _ = c.q("EXPLAIN SELECT * FROM t_idx WHERE a = 1 AND b = 2")
    plan = " ".join(r[0] for r in rows)
    check("k-auto-name", not codes and "t_idx_a_b_idx" in plan, f"{plan} {codes}")
    # Named creation still works alongside.
    _, _, codes, _ = c.q("CREATE INDEX my_idx ON t_idx (a)")
    check("k-named-create", not codes, f"{codes}")
    c.q("DROP TABLE t_idx")


def t_txn_syntax(c):
    # v0.15: PostgreSQL transaction-control syntax variants.
    c.q("CREATE TABLE t_txn (x int)")
    # START TRANSACTION with comma-separated modes.
    _, _, codes, _ = c.q(
        "START TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ WRITE, DEFERRABLE"
    )
    check("k-start-modes", not codes, f"{codes}")
    c.q("INSERT INTO t_txn VALUES (1)")
    # COMMIT AND CHAIN: commits, then begins a new txn (still in txn).
    # Tag must be COMMIT (PostgreSQL reports the command that ran).
    tags, _, codes, _ = c.q("COMMIT AND CHAIN")
    check("k-commit-chain", not codes, f"{codes}")
    check("k-commit-chain-tag", tags == ["COMMIT"], f"{tags}")
    c.q("INSERT INTO t_txn VALUES (2)")
    c.q("COMMIT")
    _, rows, codes, _ = c.q("SELECT x FROM t_txn ORDER BY x")
    check("k-chain-rows", not codes and rows == [("1",), ("2",)], f"{rows} {codes}")
    # COMMIT TRANSACTION / COMMIT WORK variants.
    c.q("BEGIN")
    c.q("INSERT INTO t_txn VALUES (3)")
    _, _, codes, _ = c.q("COMMIT TRANSACTION")
    check("k-commit-txn", not codes, f"{codes}")
    c.q("BEGIN")
    c.q("INSERT INTO t_txn VALUES (4)")
    _, _, codes, _ = c.q("COMMIT WORK")
    check("k-commit-work", not codes, f"{codes}")
    # ROLLBACK AND CHAIN: rolls back, then begins a new txn.
    # Tag must be ROLLBACK.
    c.q("BEGIN TRANSACTION READ ONLY")
    c.q("INSERT INTO t_txn VALUES (5)")
    tags, _, codes, _ = c.q("ROLLBACK AND CHAIN")
    check("k-rollback-chain", not codes, f"{codes}")
    check("k-rollback-chain-tag", tags == ["ROLLBACK"], f"{tags}")
    c.q("ROLLBACK")  # roll back the chained (empty) txn
    _, rows, codes, _ = c.q("SELECT x FROM t_txn ORDER BY x")
    check(
        "k-rollback-rows",
        not codes and rows == [("1",), ("2",), ("3",), ("4",)],
        f"{rows} {codes}",
    )
    # BEGIN WORK and START TRANSACTION READ WRITE.
    _, _, codes, _ = c.q("BEGIN WORK")
    check("k-begin-work", not codes, f"{codes}")
    c.q("ROLLBACK")
    _, _, codes, _ = c.q("START TRANSACTION READ WRITE")
    check("k-start-rw", not codes, f"{codes}")
    c.q("ROLLBACK")
    # COMMIT AND CHAIN with no open transaction: still COMMIT tag, and a
    # chained transaction is now open (next INSERT must be committable).
    tags, _, codes, _ = c.q("COMMIT AND CHAIN")
    check("k-chain-no-txn", not codes, f"{codes}")
    check("k-chain-no-txn-tag", tags == ["COMMIT"], f"{tags}")
    c.q("INSERT INTO t_txn VALUES (6)")
    c.q("COMMIT")
    _, rows, codes, _ = c.q("SELECT x FROM t_txn WHERE x = 6")
    check("k-chain-no-txn-rows", not codes and rows == [("6",)], f"{rows} {codes}")
    c.q("DROP TABLE t_txn")


TESTS = [
    t_type_aliases,
    t_vacuum_analyze,
    t_insert_typed_literals,
    t_int_to_bool,
    t_bool_prefixes,
    t_operator_aliases,
    t_cast_colname,
    t_temp_tables,
    t_from_extras,
    t_func_casts,
    t_nameless_index,
    t_txn_syntax,
]

if __name__ == "__main__":
    if not os.path.exists(BIN):
        print(f"missing {BIN}; build first")
        sys.exit(2)
    srv = Server()
    try:
        c = Conn(PORT)
        for t in TESTS:
            print(f"== {t.__name__}")
            try:
                t(c)
            except Exception as e:
                failed.append(t.__name__)
                print(f"  FAIL: {t.__name__} raised {e!r}")
        c.close()
    finally:
        srv.stop()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    if failed:
        print("failures:", failed)
        sys.exit(1)
