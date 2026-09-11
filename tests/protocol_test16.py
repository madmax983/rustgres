#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.16 (missing built-ins, cursors, TRUNCATE).

Groups:
  A. substr           -- 1-based, Unicode-aware, negative start, aliases.
  B. concat/concat_ws -- NULL skipping, heterogeneous args, empty separator.
  C. to_hex/to_oct/to_bin -- int4 (32-bit) vs bigint (64-bit) two's
     complement widths; non-negative values unpadded.
  D. sign             -- int, numeric, float8, zero variants.
  E. left/right       -- positive, negative, oversized counts; Unicode.
  F. reverse          -- Unicode-aware; NULL stays NULL.
  G. TRUNCATE         -- bare `TRUNCATE t`, `TRUNCATE TABLE t`, tag, count=0,
     transactional rollback restores rows, FK RESTRICT/CASCADE.
  H. cursors          -- DECLARE outside txn is 25001; DECLARE/FETCH NEXT/
     FETCH n/FETCH ALL/FETCH 0-at-end/CLOSE; 34000 on missing cursor;
     CLOSE ALL; MOVE; FIRST/LAST/ABSOLUTE/RELATIVE positioning;
     WITH HOLD survives COMMIT; cursors die on ROLLBACK.
  I. savepoint/cursor -- FETCH position rewinds on ROLLBACK TO SAVEPOINT;
     cursors created after the savepoint are closed by the rollback;
     cursors die in aborted subtransactions.
  J. error codes       -- arity errors (42883), unknown function (42883),
     FETCH in aborted txn (25P02).

Boots its own server on 127.0.0.1:5433 with a scratch data dir.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test16.py`.
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
        s = socket.socket()
        try:
            s.bind((HOST, PORT))
        except OSError as e:
            raise RuntimeError(f"port {PORT} already in use: {e}")
        finally:
            s.close()
        self.datadir = tempfile.mkdtemp(prefix="rg16-")
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


def tag_of(c, sql):
    tags, _, codes, _ = c.q(sql)
    return (tags[0] if tags else ""), codes


# ---------------------------------------------------------------------------
# A. substr


def t_substr(c):
    cases = [
        ("SELECT substr('hello', 2, 3)", [("ell",)]),
        ("SELECT substr('hello', 2)", [("ello",)]),
        ("SELECT substr('hello', -2, 4)", [("h",)]),
        ("SELECT substr('hello', 99)", [("",)]),
        ("SELECT substr('hello', 2, 99)", [("ello",)]),
        ("SELECT substr('héllo', 2, 3)", [("éll",)]),
        ("SELECT substring('hello' from 2 for 3)", [("ell",)]),
    ]
    for sql, want in cases:
        rows, codes = rows_of(c, sql)
        check(f"a-{sql[7:25].replace(' ', '_')}", not codes and rows == want,
              f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT substr(NULL, 1, 2)")
    check("a-null", not codes and rows == [(None,)], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# B. concat / concat_ws


def t_concat(c):
    cases = [
        ("SELECT concat('a', NULL, 1, true)", [("a1t",)]),
        ("SELECT concat('x')", [("x",)]),
        ("SELECT concat()", [("",)]),
        ("SELECT concat_ws(',', 'a', NULL, 'b')", [("a,b",)]),
        ("SELECT concat_ws(',', 'a', 'b')", [("a,b",)]),
        ("SELECT concat_ws('', 'a', 'b')", [("ab",)]),
        ("SELECT concat_ws('-', 1, 2.5, true)", [("1-2.5-t",)]),
    ]
    for sql, want in cases:
        rows, codes = rows_of(c, sql)
        check(f"b-{sql[7:22].replace(' ', '_')}", not codes and rows == want,
              f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT concat_ws(NULL, 'a', 'b')")
    check("b-null-sep", not codes and rows == [(None,)], f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT concat(NULL)")
    check("b-null-arg", not codes and rows == [("",)], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# C. to_hex / to_oct / to_bin


def t_to_radix(c):
    cases = [
        # int4: 32-bit two's complement for negatives.
        ("SELECT to_hex(-1234)", [("fffffb2e",)]),
        ("SELECT to_hex(255)", [("ff",)]),
        ("SELECT to_hex(0)", [("0",)]),
        ("SELECT to_oct(-1234)", [("37777775456",)]),
        ("SELECT to_oct(8)", [("10",)]),
        ("SELECT to_bin(-1234)",
         [("11111111111111111111101100101110",)]),
        ("SELECT to_bin(5)", [("101",)]),
        # bigint: 64-bit two's complement (width follows the type, not the
        # range: -1234::bigint is 64-bit even though it fits in int4).
        ("SELECT to_hex(-1234::bigint)", [("fffffffffffffb2e",)]),
        ("SELECT to_oct(-1234::bigint)", [("1777777777777777775456",)]),
        ("SELECT to_bin(-1234::bigint)",
         [("1111111111111111111111111111111111111111111111111111101100101110",)]),
        ("SELECT to_hex(9223372036854775807::bigint)",
         [("7fffffffffffffff",)]),
    ]
    for sql, want in cases:
        rows, codes = rows_of(c, sql)
        check(f"c-{sql[7:30].replace(' ', '_')}", not codes and rows == want,
              f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT to_hex(NULL)")
    check("c-null", not codes and rows == [(None,)], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# D. sign


def t_sign(c):
    cases = [
        ("SELECT sign(-5)", [("-1",)]),
        ("SELECT sign(0)", [("0",)]),
        ("SELECT sign(42)", [("1",)]),
        ("SELECT sign(-2.5::float8)", [("-1",)]),
        ("SELECT sign(2.5)", [("1",)]),
        ("SELECT sign(0.0::numeric)", [("0",)]),
        ("SELECT sign(-0.5::numeric)", [("-1",)]),
    ]
    for sql, want in cases:
        rows, codes = rows_of(c, sql)
        check(f"d-{sql[7:24].replace(' ', '_')}", not codes and rows == want,
              f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT sign(NULL)")
    check("d-null", not codes and rows == [(None,)], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# E. left / right


def t_left_right(c):
    cases = [
        ("SELECT left('abcdef', 2)", [("ab",)]),
        ("SELECT left('abcdef', -2)", [("abcd",)]),
        ("SELECT left('abcdef', 99)", [("abcdef",)]),
        ("SELECT left('abcdef', 0)", [("",)]),
        ("SELECT right('abcdef', 2)", [("ef",)]),
        ("SELECT right('abcdef', -2)", [("cdef",)]),
        ("SELECT right('abcdef', 99)", [("abcdef",)]),
        ("SELECT left('héllo', 2)", [("hé",)]),
        ("SELECT right('héllo', 2)", [("lo",)]),
    ]
    for sql, want in cases:
        rows, codes = rows_of(c, sql)
        check(f"e-{sql[7:24].replace(' ', '_')}", not codes and rows == want,
              f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT left(NULL, 2), right('ab', NULL)")
    check("e-null", not codes and rows == [(None, None)], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# F. reverse


def t_reverse(c):
    cases = [
        ("SELECT reverse('abc')", [("cba",)]),
        ("SELECT reverse('héllo')", [("olléh",)]),
        ("SELECT reverse('')", [("",)]),
        ("SELECT reverse('a')", [("a",)]),
    ]
    for sql, want in cases:
        rows, codes = rows_of(c, sql)
        check(f"f-{sql[7:22]}", not codes and rows == want, f"{rows} {codes}")
    rows, codes = rows_of(c, "SELECT reverse(NULL)")
    check("f-null", not codes and rows == [(None,)], f"{rows} {codes}")


# ---------------------------------------------------------------------------
# G. TRUNCATE


def t_truncate(c):
    c.q("CREATE TABLE t_tr (a int, b text)")
    c.q("INSERT INTO t_tr VALUES (1, 'x'), (2, 'y')")
    tag, codes = tag_of(c, "TRUNCATE t_tr")
    check("g-bare", not codes and tag == "TRUNCATE TABLE", f"{tag} {codes}")
    rows, _ = rows_of(c, "SELECT count(*) FROM t_tr")
    check("g-empty", rows == [("0",)], f"{rows}")
    c.q("INSERT INTO t_tr VALUES (3, 'z')")
    tag, codes = tag_of(c, "TRUNCATE TABLE t_tr")
    check("g-table-kw", not codes and tag == "TRUNCATE TABLE", f"{tag} {codes}")
    # Transactional: rollback restores the rows.
    c.q("INSERT INTO t_tr VALUES (4, 'w')")
    c.q("BEGIN")
    c.q("TRUNCATE t_tr")
    rows, _ = rows_of(c, "SELECT count(*) FROM t_tr")
    check("g-in-txn", rows == [("0",)], f"{rows}")
    c.q("ROLLBACK")
    rows, _ = rows_of(c, "SELECT count(*) FROM t_tr")
    check("g-rollback", rows == [("1",)], f"{rows}")
    # Missing table.
    _, codes = rows_of(c, "TRUNCATE nope_missing")
    check("g-missing", codes == ["42P01"], f"{codes}")
    # FK RESTRICT blocks; CASCADE truncates both.
    c.q("CREATE TABLE t_par (a int PRIMARY KEY)")
    c.q("CREATE TABLE t_ch (a int REFERENCES t_par(a))")
    c.q("INSERT INTO t_par VALUES (1)")
    c.q("INSERT INTO t_ch VALUES (1)")
    _, codes = rows_of(c, "TRUNCATE t_par")
    check("g-fk-restrict", codes == ["2BP01"], f"{codes}")
    tag, codes = tag_of(c, "TRUNCATE t_par CASCADE")
    check("g-fk-cascade", not codes and tag == "TRUNCATE TABLE",
          f"{tag} {codes}")
    rows, _ = rows_of(c, "SELECT count(*) FROM t_ch")
    check("g-cascade-child", rows == [("0",)], f"{rows}")
    c.q("DROP TABLE t_ch")
    c.q("DROP TABLE t_par")
    c.q("DROP TABLE t_tr")


# ---------------------------------------------------------------------------
# H. cursors


def t_cursors(c):
    c.q("CREATE TABLE t_cur (a int)")
    c.q("INSERT INTO t_cur VALUES (10), (20), (30), (40)")
    # DECLARE outside a transaction is rejected (25001), like Postgres.
    _, codes = rows_of(c, "DECLARE c1 CURSOR FOR SELECT 1")
    check("h-outside-txn", codes == ["25001"], f"{codes}")
    c.q("BEGIN")
    tag, codes = tag_of(c, "DECLARE c1 CURSOR FOR SELECT a FROM t_cur ORDER BY a")
    check("h-declare", not codes and tag == "DECLARE CURSOR", f"{tag} {codes}")

    def fetch(sql):
        tags, rows, codes, _ = c.q(sql)
        return (tags[0] if tags else ""), rows, codes

    tag, rows, codes = fetch("FETCH NEXT FROM c1")
    check("h-next", not codes and tag == "FETCH 1" and rows == [("10",)],
          f"{tag} {rows} {codes}")
    tag, rows, codes = fetch("FETCH 2 FROM c1")
    check("h-count", not codes and tag == "FETCH 2"
          and rows == [("20",), ("30",)], f"{tag} {rows} {codes}")
    tag, rows, codes = fetch("FETCH ALL FROM c1")
    check("h-all", not codes and tag == "FETCH 1" and rows == [("40",)],
          f"{tag} {rows} {codes}")
    tag, rows, codes = fetch("FETCH NEXT FROM c1")
    check("h-eof", not codes and tag == "FETCH 0" and rows == [],
          f"{tag} {rows} {codes}")
    # Positioning directions.
    tag, rows, codes = fetch("MOVE BACKWARD 3 FROM c1")
    check("h-move", not codes and tag == "MOVE 3", f"{tag} {codes}")
    tag, rows, codes = fetch("FETCH NEXT FROM c1")
    check("h-after-move", not codes and rows == [("30",)], f"{rows} {codes}")
    _, rows, codes = fetch("FETCH FIRST FROM c1")
    check("h-first", not codes and rows == [("10",)], f"{rows} {codes}")
    _, rows, codes = fetch("FETCH LAST FROM c1")
    check("h-last", not codes and rows == [("40",)], f"{rows} {codes}")
    _, rows, codes = fetch("FETCH ABSOLUTE 2 FROM c1")
    check("h-absolute", not codes and rows == [("20",)], f"{rows} {codes}")
    _, rows, codes = fetch("FETCH RELATIVE -1 FROM c1")
    check("h-relative", not codes and rows == [("10",)], f"{rows} {codes}")
    tag, codes = tag_of(c, "CLOSE c1")
    check("h-close", not codes and tag == "CLOSE CURSOR", f"{tag} {codes}")
    _, codes = rows_of(c, "FETCH NEXT FROM c1")
    check("h-missing", codes == ["34000"], f"{codes}")
    # CLOSE ALL.
    c.q("DECLARE c2 CURSOR FOR SELECT 1")
    c.q("DECLARE c3 CURSOR FOR SELECT 2")
    tag, codes = tag_of(c, "CLOSE ALL")
    check("h-close-all", not codes and tag == "CLOSE CURSOR", f"{tag} {codes}")
    _, codes = rows_of(c, "FETCH NEXT FROM c2")
    check("h-closed-all", codes == ["34000"], f"{codes}")
    c.q("COMMIT")
    # WITH HOLD survives COMMIT (but not ROLLBACK, like Postgres); plain
    # cursors die on COMMIT.
    tag, codes = tag_of(c, "DECLARE ch CURSOR WITH HOLD FOR SELECT 42")
    check("h-hold-declare", not codes and tag == "DECLARE CURSOR",
          f"{tag} {codes}")
    c.q("BEGIN")
    c.q("DECLARE cp CURSOR FOR SELECT 1")
    c.q("COMMIT")
    _, codes = rows_of(c, "FETCH NEXT FROM cp")
    check("h-plain-dies", codes == ["34000"], f"{codes}")
    tag, rows, codes = fetch("FETCH NEXT FROM ch")
    check("h-hold-survives", not codes and tag == "FETCH 1"
          and rows == [("42",)], f"{tag} {rows} {codes}")
    # ROLLBACK destroys even WITH HOLD cursors.
    c.q("DECLARE ch2 CURSOR WITH HOLD FOR SELECT 7")
    c.q("BEGIN")
    c.q("ROLLBACK")
    _, codes = rows_of(c, "FETCH NEXT FROM ch2")
    check("h-hold-dies-on-rollback", codes == ["34000"], f"{codes}")
    c.q("CLOSE ch")
    c.q("DROP TABLE t_cur")


# ---------------------------------------------------------------------------
# I. savepoint / cursor interaction


def t_savepoint_cursors(c):
    c.q("CREATE TABLE t_sv (a int)")
    c.q("BEGIN")
    c.q("INSERT INTO t_sv VALUES (5)")
    c.q("INSERT INTO t_sv VALUES (10)")
    c.q("DECLARE sv CURSOR FOR SELECT a FROM t_sv ORDER BY a")
    _, rows, codes, _ = c.q("FETCH NEXT FROM sv")
    check("i-first", not codes and rows == [("5",)], f"{rows} {codes}")
    c.q("SAVEPOINT x")
    _, rows, codes, _ = c.q("FETCH NEXT FROM sv")
    check("i-second", not codes and rows == [("10",)], f"{rows} {codes}")
    c.q("ROLLBACK TO SAVEPOINT x")
    # FETCH effects are not rolled back: position restored to the savepoint.
    _, rows, codes, _ = c.q("FETCH NEXT FROM sv")
    check("i-rewound", not codes and rows == [("10",)], f"{rows} {codes}")
    # Cursors created after the savepoint die on rollback to it.
    c.q("SAVEPOINT y")
    c.q("DECLARE sv2 CURSOR FOR SELECT 1")
    c.q("ROLLBACK TO SAVEPOINT y")
    _, codes = rows_of(c, "FETCH NEXT FROM sv2")
    check("i-created-after", codes == ["34000"], f"{codes}")
    c.q("RELEASE y")
    # Aborted subtransaction kills cursors declared inside it.
    c.q("SAVEPOINT z")
    c.q("DECLARE sv3 CURSOR FOR SELECT 1")
    c.q("SELECT 1/0")
    c.q("ROLLBACK TO SAVEPOINT z")
    _, codes = rows_of(c, "FETCH NEXT FROM sv3")
    check("i-aborted-subtxn", codes == ["34000"], f"{codes}")
    c.q("CLOSE sv")
    c.q("ROLLBACK")
    c.q("DROP TABLE t_sv")


# ---------------------------------------------------------------------------
# J. error codes


def t_error_codes(c):
    _, codes = rows_of(c, "SELECT substr('a')")
    check("j-arity", codes == ["42883"], f"{codes}")
    _, codes = rows_of(c, "SELECT nosuchfunc16('a')")
    check("j-unknown", codes == ["42883"], f"{codes}")
    _, codes = rows_of(c, "FETCH NEXT FROM missing_cur_xyz")
    check("j-missing-cursor", codes == ["34000"], f"{codes}")
    # FETCH inside an aborted transaction is rejected (25P02).
    c.q("BEGIN")
    c.q("SELECT 1/0")
    _, codes = rows_of(c, "FETCH NEXT FROM whatever")
    check("j-aborted", codes == ["25P02"], f"{codes}")
    c.q("ROLLBACK")


TESTS = [
    t_substr,
    t_concat,
    t_to_radix,
    t_sign,
    t_left_right,
    t_reverse,
    t_truncate,
    t_cursors,
    t_savepoint_cursors,
    t_error_codes,
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
