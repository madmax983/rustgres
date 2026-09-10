#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.7 (richer types, casts, operators, built-ins).

Covers type parse/format round trips (smallint, bigint, real, numeric,
date, timestamp, timestamptz, bytea, uuid), cast success/failure with
SQLSTATEs, the numeric promotion matrix, the three-valued boolean truth
table, scalar built-ins (string/math/datetime/conditional), date/timestamp
arithmetic, LIKE/ILIKE/BETWEEN/concat, explicit NULL ordering, aggregates
over NULLs (incl. string_agg and DISTINCT), and WAL/checkpoint replay of
every new type.

Each test boots a fresh server on a scratch data dir (like the v0.6
suite), so state can never leak between tests.

Usage: build the server first (`cargo build`), then `python3 tests/protocol_test7.py`.
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

_next_port = [55533]


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
        self.data_dir = tempfile.mkdtemp(prefix="rg7_")
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
            wait_for_port_free(self.port)

    def kill9(self):
        if self.proc is not None:
            self.proc.send_signal(signal.SIGKILL)
            self.proc.wait(timeout=10)
            self.proc = None
            wait_for_port_free(self.port)

    def cleanup(self):
        self.stop()
        shutil.rmtree(self.data_dir, ignore_errors=True)


def fresh_server():
    srv = Server(alloc_port())
    srv.start()
    return srv


def one(c, sql):
    """Run a single-row single-column SELECT; return the text value (or None)."""
    tags, rows, codes = c.q(sql)
    assert codes == [], f"{sql}: unexpected error {codes}"
    assert len(rows) == 1 and len(rows[0]) == 1, f"{sql}: {rows}"
    return rows[0][0]


def errcode(c, sql):
    """Run SQL; return the first error code (or None if it succeeded)."""
    _, _, codes = c.q(sql)
    return codes[0] if codes else None


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def t_type_roundtrip():
    print("== type parse/format round trip ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("""CREATE TABLE alltypes(
          s SMALLINT, i INT, b BIGINT, r REAL, d DOUBLE PRECISION, n NUMERIC,
          t TEXT, bo BOOLEAN, dt DATE, ts TIMESTAMP, tz TIMESTAMPTZ,
          by BYTEA, u UUID)""")
        c.q("""INSERT INTO alltypes VALUES (
          42, 2147483647, 9223372036854775807, 1.5, 2.5,
          '123456789012345678901234567890.12345678',
          'hello', true, '2026-09-10', '2026-09-10 12:34:56.789012',
          '2026-09-10 12:34:56+02', '\\xdeadbeef',
          'a0b1c2d3-e4f5-4678-9abc-def012345678')""")
        _, rows, codes = c.q("SELECT s,i,b,r,d,n,t,bo,dt,ts,tz,by,u FROM alltypes")
        check("no error", codes == [], str(codes))
        r = rows[0]
        check("smallint", r[0] == "42", r[0])
        check("int", r[1] == "2147483647", r[1])
        check("bigint", r[2] == "9223372036854775807", r[2])
        check("real", r[3] == "1.5", r[3])
        check("double", r[4] == "2.5", r[4])
        check("numeric", r[5] == "123456789012345678901234567890.12345678", r[5])
        check("text", r[6] == "hello", r[6])
        check("bool", r[7] == "t", r[7])
        check("date", r[8] == "2026-09-10", r[8])
        check("timestamp", r[9] == "2026-09-10 12:34:56.789012", r[9])
        check("timestamptz normalizes to UTC",
              r[10] == "2026-09-10 10:34:56+00", r[10])
        check("bytea", r[11] == "\\xdeadbeef", r[11])
        check("uuid", r[12] == "a0b1c2d3-e4f5-4678-9abc-def012345678", r[12])
        # Typed literals.
        check("DATE literal", one(c, "SELECT DATE '2000-02-29'") == "2000-02-29")
        check("TIMESTAMP literal",
              one(c, "SELECT TIMESTAMP '2000-01-01 00:00:00'") == "2000-01-01 00:00:00")
        check("timestamptz Z suffix",
              one(c, "SELECT TIMESTAMPTZ '2000-01-01 00:00:00Z'") == "2000-01-01 00:00:00+00")
        check("UUID literal",
              one(c, "SELECT UUID 'A0B1C2D3-E4F5-4678-9ABC-DEF012345678'")
              == "a0b1c2d3-e4f5-4678-9abc-def012345678")
        c.close()
    finally:
        srv.cleanup()


def t_casts():
    print("== casts: success, failure, SQLSTATEs ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        check("text->int", one(c, "SELECT '123'::int") == "123")
        check("int->text", one(c, "SELECT 123::text") == "123")
        check("text->date", one(c, "SELECT '2026-01-01'::date") == "2026-01-01")
        check("CAST() syntax", one(c, "SELECT CAST('2026-01-01' AS date)") == "2026-01-01")
        check("int->bigint", one(c, "SELECT 5::bigint") == "5")
        check("int->smallint", one(c, "SELECT 5::smallint") == "5")
        check("int->real", one(c, "SELECT 5::real") == "5")
        check("int->numeric", one(c, "SELECT 5::numeric") == "5")
        check("numeric round half away",
              one(c, "SELECT 2.5::numeric::int") == "3"
              and one(c, "SELECT (-2.5)::numeric::int") == "-3")
        check("float->int rounds", one(c, "SELECT CAST(1.5 AS int)") == "2")
        check("bool text forms",
              one(c, "SELECT 't'::bool") == "t"
              and one(c, "SELECT 'yes'::bool") == "t"
              and one(c, "SELECT 'off'::bool") == "f"
              and one(c, "SELECT '0'::bool") == "f")
        check("bool->text", one(c, "SELECT true::text") == "true")
        check("text->bytea", one(c, "SELECT 'xy'::bytea") == "\\x7879")
        check("bytea->text", one(c, "SELECT 'hi'::bytea::text") == "\\x6869")
        check("date->text", one(c, "SELECT '2026-01-02'::date::text") == "2026-01-02")
        check("date->timestamp",
              one(c, "SELECT '2026-01-02'::date::timestamp") == "2026-01-02 00:00:00")
        check("timestamp->date",
              one(c, "SELECT '2026-01-02 15:04:05'::timestamp::date") == "2026-01-02")
        check("NULL cast", c.q("SELECT NULL::int")[1] == [[None]])
        # Failures.
        check("bad int text -> 22P02", errcode(c, "SELECT 'abc'::int") == "22P02")
        check("bad date text -> 22P02", errcode(c, "SELECT '2026-13-45'::date") == "22P02")
        check("bad uuid text -> 22P02", errcode(c, "SELECT 'nope'::uuid") == "22P02")
        check("bad bytea text -> 22P02", errcode(c, "SELECT '\\xzz'::bytea") == "22P02")
        check("bad bool text -> 22P02", errcode(c, "SELECT 'maybe'::bool") == "22P02")
        check("div by zero -> 22012", errcode(c, "SELECT 1/0") == "22012")
        check("numeric div by zero -> 22012",
              errcode(c, "SELECT 1::numeric/0") == "22012")
        check("float div by zero -> 22012 (PG behavior)",
              errcode(c, "SELECT 1.0/0.0") == "22012")
        check("int overflow -> 22003", errcode(c, "SELECT 2147483647 + 1") == "22003")
        check("bigint overflow -> 22003",
              errcode(c, "SELECT 9223372036854775807::bigint + 1") == "22003")
        check("smallint literal overflow -> 22003",
              errcode(c, "SELECT 100000::smallint") == "22003")
        check("int->date -> 42846", errcode(c, "SELECT 1::date") == "42846")
        c.q("CREATE TABLE ci2(a INT)")
        check("insert bad int text -> 22P02",
              errcode(c, "INSERT INTO ci2 VALUES ('xx')") == "22P02")
        c.q("CREATE TABLE cd(a DATE)")
        check("insert bad date text -> 22P02",
              errcode(c, "INSERT INTO cd VALUES ('yesterday')") == "22P02")
        c.close()
    finally:
        srv.cleanup()


def t_promotion():
    print("== numeric promotion matrix ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        check("smallint+smallint->integer",
              one(c, "SELECT 30000::smallint + 30000::smallint") == "60000")
        check("int+bigint->bigint", one(c, "SELECT 1 + 2::bigint") == "3")
        check("int+real->real", one(c, "SELECT 1 + 1.5::real") == "2.5")
        check("int+double->double", one(c, "SELECT 1 + 2.5") == "3.5")
        check("int+numeric->numeric", one(c, "SELECT 1 + 1.5::numeric") == "2.5")
        check("real+double->double", one(c, "SELECT 1.5::real + 2.5") == "4")
        check("int division truncates", one(c, "SELECT 7/2") == "3")
        check("double division", one(c, "SELECT 7.0/2") == "3.5")
        check("numeric division", one(c, "SELECT 7::numeric/2") == "3.5")
        check("int power", one(c, "SELECT 2^3") == "8")
        check("numeric power exact", one(c, "SELECT 1.5::numeric^2") == "2.25")
        check("float power", one(c, "SELECT 2.0^0.5") == "1.4142135623730951")
        check("mod ints", one(c, "SELECT 10 % 3") == "1")
        check("mod numeric", one(c, "SELECT 10.5::numeric % 3") == "1.5")
        check("mod negative dividend",
              one(c, "SELECT (-10) % 3") == "-1")
        check("unary minus binds tighter than ^",
              one(c, "SELECT -2^2") == "4")  # PG parses as (-2)^2
        check("bigint literal", one(c, "SELECT 9223372036854775807") == "9223372036854775807")
        check("numeric literal stays exact",
              one(c, "SELECT 0.1::numeric + 0.2::numeric") == "0.3")
        c.close()
    finally:
        srv.cleanup()


def t_bool_logic():
    print("== three-valued boolean logic ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        cases = [
            ("TRUE AND TRUE", "t"), ("TRUE AND FALSE", "f"),
            ("TRUE AND NULL", None), ("FALSE AND NULL", "f"),
            ("NULL AND NULL", None),
            ("TRUE OR TRUE", "t"), ("TRUE OR FALSE", "t"),
            ("TRUE OR NULL", "t"), ("FALSE OR NULL", None),
            ("NULL OR NULL", None),
            ("NOT TRUE", "f"), ("NOT FALSE", "t"), ("NOT NULL", None),
            ("NULL = NULL", None), ("NULL <> 1", None),
            ("1 = NULL", None), ("NULL IS NULL", "t"),
            ("1 IS NULL", "f"), ("NULL IS NOT NULL", "f"),
        ]
        for sql, want in cases:
            got = one(c, f"SELECT {sql}")
            check(f"{sql} -> {want}", got == want, f"got {got}")
        check("IS TRUE", one(c, "SELECT (NULL IS TRUE)") == "f"
              and one(c, "SELECT (TRUE IS TRUE)") == "t")
        check("IS FALSE", one(c, "SELECT (FALSE IS FALSE)") == "t")
        check("IS UNKNOWN", one(c, "SELECT (NULL IS UNKNOWN)") == "t"
              and one(c, "SELECT (TRUE IS UNKNOWN)") == "f")
        check("IS NOT TRUE", one(c, "SELECT (NULL IS NOT TRUE)") == "t")
        c.q("CREATE TABLE wnull(a integer)")
        c.q("INSERT INTO wnull VALUES (1), (NULL), (3)")
        check("WHERE filters NULL",
              c.q("SELECT a FROM wnull WHERE a > 1")[1] == [["3"]])
        check("WHERE IS NULL",
              c.q("SELECT a FROM wnull WHERE a IS NULL")[1] == [[None]])
        c.close()
    finally:
        srv.cleanup()


def t_functions():
    print("== scalar built-ins ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        # String.
        check("upper/lower", one(c, "SELECT upper('AbC')") == "ABC"
              and one(c, "SELECT lower('AbC')") == "abc")
        check("length", one(c, "SELECT length('hello')") == "5"
              and one(c, "SELECT char_length('héllo')") == "5")
        check("substring", one(c, "SELECT substring('hello' from 2 for 3)") == "ell"
              and one(c, "SELECT substring('hello', 2)") == "ello")
        check("trim", one(c, "SELECT trim('  hi  ')") == "hi"
              and one(c, "SELECT trim(both 'x' from 'xxhixx')") == "hi"
              and one(c, "SELECT trim(leading 'x' from 'xxhi')") == "hi"
              and one(c, "SELECT trim(trailing 'x' from 'hixx')") == "hi")
        check("position", one(c, "SELECT position('ll' in 'hello')") == "3"
              and one(c, "SELECT position('z' in 'hello')") == "0")
        check("replace", one(c, "SELECT replace('hello','l','L')") == "heLLo")
        check("split_part", one(c, "SELECT split_part('a,b,c', ',', 2)") == "b"
              and one(c, "SELECT split_part('a,b,c', ',', 9)") == "")
        check("concat operator", one(c, "SELECT 'a' || 'b'") == "ab"
              and one(c, "SELECT 'a' || NULL") is None)
        check("string fns NULL in NULL out",
              one(c, "SELECT upper(NULL)") is None
              and one(c, "SELECT length(NULL)") is None)
        check("string fn wrong type -> 42883",
              errcode(c, "SELECT upper(1)") == "42883")
        # Math.
        check("abs", one(c, "SELECT abs(-5)") == "5"
              and one(c, "SELECT abs(-5.5::numeric)") == "5.5")
        check("round", one(c, "SELECT round(2.5::numeric)") == "3"
              and one(c, "SELECT round(2.4::numeric)") == "2")
        check("floor/ceil", one(c, "SELECT floor(2.9)") == "2"
              and one(c, "SELECT ceil(2.1)") == "3")
        check("sqrt", one(c, "SELECT sqrt(2.0)") == "1.4142135623730951"
              and one(c, "SELECT sqrt(16::numeric)") == "4")
        # sqrt(float8 negative) -> NaN in PG; numeric negative -> 2201F.
        check("sqrt negative -> 2201F", errcode(c, "SELECT sqrt(-1::numeric)") == "2201F")
        check("sqrt float negative -> NaN",
              one(c, "SELECT sqrt(-1.0)") == "NaN")
        check("mod()", one(c, "SELECT mod(10, 3)") == "1")
        check("mod by zero -> 22012", errcode(c, "SELECT mod(10, 0)") == "22012")
        check("power()", one(c, "SELECT power(2, 10)") == "1024")
        # Datetime.
        check("current_date format",
              len(one(c, "SELECT current_date")) == 10)
        check("now() format", one(c, "SELECT now()").endswith("+00"))
        check("date_trunc day",
              one(c, "SELECT date_trunc('day', '2026-09-10 12:34:56'::timestamp)")
              == "2026-09-10 00:00:00")
        # date_trunc on a date returns timestamp (Postgres behavior).
        check("date_trunc month",
              one(c, "SELECT date_trunc('month', '2026-09-10'::date)") == "2026-09-01 00:00:00")
        check("extract year", one(c, "SELECT extract(year from '2026-09-10'::date)") == "2026")
        check("extract epoch",
              one(c, "SELECT extract(epoch from '1970-01-02'::date)") == "86400")
        check("extract dow", one(c, "SELECT extract(dow from '2026-09-10'::date)") == "4")
        # Conditional.
        check("coalesce", one(c, "SELECT coalesce(NULL, NULL, 3)") == "3"
              and one(c, "SELECT coalesce(NULL)") is None)
        check("nullif", one(c, "SELECT nullif(1, 1)") is None
              and one(c, "SELECT nullif(1, 2)") == "1")
        check("greatest/least ignore NULLs",
              one(c, "SELECT greatest(1, NULL, 3)") == "3"
              and one(c, "SELECT least(1, NULL, 3)") == "1"
              and one(c, "SELECT greatest(NULL, NULL)") is None)
        check("unknown function -> 42883",
              errcode(c, "SELECT nosuchfn(1)") == "42883")
        check("wrong arity -> 42883",
              errcode(c, "SELECT upper('a','b')") == "42883"
              and errcode(c, "SELECT split_part('a', ',')") == "42883")
        c.close()
    finally:
        srv.cleanup()


def t_datetime_arith():
    print("== date/timestamp arithmetic ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        check("date + int", one(c, "SELECT '2000-01-01'::date + 30") == "2000-01-31")
        check("int + date", one(c, "SELECT 30 + '2000-01-01'::date") == "2000-01-31")
        check("date - int", one(c, "SELECT '2000-01-31'::date - 30") == "2000-01-01")
        check("date - date -> days",
              one(c, "SELECT '2000-01-31'::date - '2000-01-01'::date") == "30")
        check("date - date negative",
              one(c, "SELECT '2000-01-01'::date - '2000-01-31'::date") == "-30")
        check("date + 0", one(c, "SELECT '2026-09-10'::date + 0") == "2026-09-10")
        # No INTERVAL in v0.7: timestamp arithmetic is undefined (42883).
        check("timestamp - timestamp -> 42883 (no interval in v0.7)",
              errcode(c, "SELECT '2026-01-02'::timestamp - '2026-01-01'::timestamp")
              == "42883")
        check("timestamp + int -> 42883",
              errcode(c, "SELECT '2026-01-01'::timestamp + 1") == "42883")
        check("date vs timestamp compare",
              one(c, "SELECT '2026-01-02'::date > '2026-01-01'::timestamp") == "t")
        c.q("CREATE TABLE odt(d date)")
        c.q("INSERT INTO odt VALUES ('2026-01-03'), ('2026-01-01')")
        check("ORDER BY date",
              c.q("SELECT d FROM odt ORDER BY d")[1]
              == [["2026-01-01"], ["2026-01-03"]])
        c.close()
    finally:
        srv.cleanup()


def t_operators():
    print("== LIKE/ILIKE/BETWEEN/concat ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        check("LIKE %", one(c, "SELECT 'hello' LIKE 'h%'") == "t")
        check("LIKE _", one(c, "SELECT 'hello' LIKE 'h_llo'") == "t")
        check("LIKE no match", one(c, "SELECT 'hello' LIKE 'z%'") == "f")
        check("LIKE escape", one(c, "SELECT '100%' LIKE '100\\%'") == "t")
        check("ILIKE case-insensitive",
              one(c, "SELECT 'Hello' ILIKE 'h%'") == "t")
        check("NOT LIKE", one(c, "SELECT 'hello' NOT LIKE 'z%'") == "t")
        check("LIKE NULL -> NULL", one(c, "SELECT NULL LIKE 'a%'") is None)
        check("BETWEEN", one(c, "SELECT 5 BETWEEN 1 AND 10") == "t"
              and one(c, "SELECT 5 NOT BETWEEN 1 AND 10") == "f"
              and one(c, "SELECT 'b' BETWEEN 'a' AND 'c'") == "t")
        check("concat ints", one(c, "SELECT 1 || 2") == "12")
        check("concat bytea", one(c, "SELECT '\\xaa'::bytea || '\\xbb'::bytea")
              == "\\xaabb")
        check("bytea = bytea", one(c, "SELECT '\\xaa'::bytea = '\\xaa'::bytea") == "t")
        check("uuid = uuid",
              one(c, "SELECT 'a0b1c2d3-e4f5-4678-9abc-def012345678'::uuid = "
                     "'A0B1C2D3-E4F5-4678-9ABC-DEF012345678'::uuid") == "t")
        check("^ binds tighter than *", one(c, "SELECT 2 * 3^2") == "18")
        c.close()
    finally:
        srv.cleanup()


def t_null_ordering():
    print("== explicit NULL ordering ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE nl(a INT)")
        c.q("INSERT INTO nl VALUES (2), (NULL), (1)")
        check("ASC defaults NULLS LAST",
              c.q("SELECT a FROM nl ORDER BY a")[1] == [["1"], ["2"], [None]])
        check("DESC defaults NULLS FIRST",
              c.q("SELECT a FROM nl ORDER BY a DESC")[1] == [[None], ["2"], ["1"]])
        check("ASC NULLS FIRST",
              c.q("SELECT a FROM nl ORDER BY a ASC NULLS FIRST")[1]
              == [[None], ["1"], ["2"]])
        check("DESC NULLS LAST",
              c.q("SELECT a FROM nl ORDER BY a DESC NULLS LAST")[1]
              == [["2"], ["1"], [None]])
        c.close()
    finally:
        srv.cleanup()


def t_agg_nulls():
    print("== aggregates over NULLs, DISTINCT, string_agg ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE ag(a INT, b TEXT)")
        c.q("INSERT INTO ag VALUES (1,'x'), (NULL,'y'), (2,'x'), (NULL,NULL), (3,'z')")
        check("count(*) counts rows", one(c, "SELECT count(*) FROM ag") == "5")
        check("count(a) skips NULLs", one(c, "SELECT count(a) FROM ag") == "3")
        check("sum ignores NULLs", one(c, "SELECT sum(a) FROM ag") == "6")
        check("avg ignores NULLs", one(c, "SELECT avg(a) FROM ag") == "2")
        check("min/max ignore NULLs",
              one(c, "SELECT min(a) FROM ag") == "1"
              and one(c, "SELECT max(a) FROM ag") == "3")
        check("count is bigint",
              one(c, "SELECT count(*)::text FROM ag") == "5")
        check("sum(DISTINCT a)", one(c, "SELECT sum(DISTINCT a) FROM ag") == "6")
        c.q("INSERT INTO ag VALUES (1,'dup')")
        check("sum(DISTINCT a) dedupes",
              one(c, "SELECT sum(DISTINCT a) FROM ag") == "6")
        check("count(DISTINCT b)", one(c, "SELECT count(DISTINCT b) FROM ag") == "4")
        check("string_agg",
              one(c, "SELECT string_agg(b, ',') FROM ag") == "x,y,x,z,dup")
        check("string_agg skips NULLs",
              one(c, "SELECT string_agg(b, ',') FROM ag WHERE a IS NULL") == "y")
        check("string_agg empty -> NULL",
              one(c, "SELECT string_agg(b, ',') FROM ag WHERE 1=0") is None)
        check("string_agg(DISTINCT b, ',')",
              one(c, "SELECT string_agg(DISTINCT b, ',') FROM ag") == "x,y,z,dup")
        check("sum all-NULL -> NULL",
              one(c, "SELECT sum(a) FROM ag WHERE 1=0") is None)
        check("avg all-NULL -> NULL",
              one(c, "SELECT avg(a) FROM ag WHERE 1=0") is None)
        check("min all-NULL -> NULL",
              one(c, "SELECT min(b) FROM ag WHERE 1=0") is None)
        check("count all-NULL -> 0",
              one(c, "SELECT count(a) FROM ag WHERE 1=0") == "0")
        check("group by with NULL group",
              c.q("SELECT a, count(*) FROM ag GROUP BY a ORDER BY a NULLS LAST")[1]
              == [["1", "2"], ["2", "1"], ["3", "1"], [None, "2"]])
        c.q("CREATE TABLE agn(n numeric)")
        c.q("INSERT INTO agn VALUES ('1.5'), ('2.25')")
        check("aggregates over new types",
              one(c, "SELECT sum(n) FROM agn") == "3.75")
        c.close()
    finally:
        srv.cleanup()


def t_wal_new_types():
    print("== WAL/checkpoint replay of every new type ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("""CREATE TABLE wt(
          s SMALLINT, i INT, b BIGINT, r REAL, d DOUBLE PRECISION, n NUMERIC,
          t TEXT, bo BOOLEAN, dt DATE, ts TIMESTAMP, tz TIMESTAMPTZ,
          by BYTEA, u UUID)""")
        c.q("""INSERT INTO wt VALUES (
          -7, -2147483648, -9223372036854775808, -0.5, 1e10,
          -99999999999999999999999999999.99999,
          'wal-seed', false, '1999-12-31', '1999-12-31 23:59:59.999999',
          '2000-06-01 00:00:00-05', '\\x00ff', '00000000-0000-0000-0000-000000000000')""")
        check("CHECKPOINT", c.q("CHECKPOINT")[0] == ["CHECKPOINT"])
        c.q("INSERT INTO wt VALUES (8, 9, 10, 1.25, 2.5, 3.75, 'post-ckpt', true,"
            " '2026-09-10', '2026-09-10 00:00:01', '2026-09-10 00:00:01+00',"
            " '\\xab', 'ffffffff-ffff-ffff-ffff-ffffffffffff')")
        c.close()
        srv.kill9()
        srv.start()
        c = Conn(srv.port)
        _, rows, codes = c.q("SELECT s,i,b,r,d,n,t,bo,dt,ts,tz,by,u FROM wt ORDER BY s")
        check("replayed after kill -9", codes == [] and len(rows) == 2, str((codes, rows)))
        r0, r1 = rows
        check("row 1 smallint", r0[0] == "-7", r0[0])
        check("row 1 int min", r0[1] == "-2147483648", r0[1])
        check("row 1 bigint min", r0[2] == "-9223372036854775808", r0[2])
        check("row 1 real", r0[3] == "-0.5", r0[3])
        check("row 1 double", r0[4] == "10000000000", r0[4])
        check("row 1 numeric", r0[5] == "-99999999999999999999999999999.99999", r0[5])
        check("row 1 text", r0[6] == "wal-seed", r0[6])
        check("row 1 bool", r0[7] == "f", r0[7])
        check("row 1 date", r0[8] == "1999-12-31", r0[8])
        check("row 1 timestamp", r0[9] == "1999-12-31 23:59:59.999999", r0[9])
        check("row 1 timestamptz", r0[10] == "2000-06-01 05:00:00+00", r0[10])
        check("row 1 bytea", r0[11] == "\\x00ff", r0[11])
        check("row 1 uuid", r0[12] == "00000000-0000-0000-0000-000000000000", r0[12])
        check("row 2 (post-checkpoint WAL)",
              r1[0] == "8" and r1[6] == "post-ckpt"
              and r1[12] == "ffffffff-ffff-ffff-ffff-ffffffffffff", str(r1))
        c.close()
    finally:
        srv.cleanup()


def t_old_data_dir_refused():
    print("== old data directory is loudly refused ==")
    port = alloc_port()
    d = tempfile.mkdtemp(prefix="rg7_old_")
    try:
        # A v0.6-style WAL header: magic RGSWAL02 + base_lsn.
        with open(os.path.join(d, "wal.log"), "wb") as f:
            f.write(b"RGSWAL02" + (0).to_bytes(8, "big"))
        env = dict(os.environ, RUSTGRES_DATA_DIR=d)
        proc = subprocess.Popen([BIN, f"--port={port}"], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            # The server must refuse to start, never open the port.
            check("v0.6 wal.log refused",
                  not wait_for_port(port, timeout=3.0))
            _, err = proc.communicate(timeout=5)
            check("refusal mentions unreadable older data",
                  b"cannot read older data" in err, err.decode()[:200])
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)
        # Same for a v0.6-style checkpoint.
        with open(os.path.join(d, "wal.log"), "wb") as f:
            pass  # empty WAL is fine
        with open(os.path.join(d, "checkpoint.dat"), "wb") as f:
            f.write(b"RGSCHK02" + (2).to_bytes(4, "big"))
        proc = subprocess.Popen([BIN, f"--port={port}"], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            check("v0.6 checkpoint refused",
                  not wait_for_port(port, timeout=3.0))
            _, err = proc.communicate(timeout=5)
            check("checkpoint refusal mentions older checkpoint",
                  b"older rustgres" in err, err.decode()[:200])
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)
    finally:
        shutil.rmtree(d, ignore_errors=True)


def t_mvcc_new_types():
    print("== MVCC / JOIN / GROUP BY over new types ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE m(id INT, n NUMERIC, d DATE, u UUID)")
        c.q("INSERT INTO m VALUES (1, 10.5, '2026-01-01', "
            "'a0b1c2d3-e4f5-4678-9abc-def012345678')")
        c.q("INSERT INTO m VALUES (2, 20.25, '2026-06-15', "
            "'ffffffff-ffff-ffff-ffff-ffffffffffff')")
        c.q("BEGIN")
        c.q("UPDATE m SET n = n * 2 WHERE id = 1")
        check("own txn sees doubled numeric",
              one(c, "SELECT n FROM m WHERE id = 1") == "21")
        c.q("ROLLBACK")
        check("rollback restores numeric",
              one(c, "SELECT n FROM m WHERE id = 1") == "10.5")
        check("join on date",
              c.q("SELECT a.id FROM m a JOIN m b ON a.d = b.d WHERE a.id = 2")[1]
              == [["2"]])
        check("group by date",
              c.q("SELECT d, count(*) FROM m GROUP BY d ORDER BY d")[1]
              == [["2026-01-01", "1"], ["2026-06-15", "1"]])
        check("distinct uuid",
              c.q("SELECT DISTINCT u FROM m ORDER BY u")[1]
              == [["a0b1c2d3-e4f5-4678-9abc-def012345678"],
                  ["ffffffff-ffff-ffff-ffff-ffffffffffff"]])
        check("numeric in WHERE",
              c.q("SELECT id FROM m WHERE n > 15")[1] == [["2"]])
        check("update date column",
              c.q("UPDATE m SET d = '2027-01-01' WHERE id = 2")[0] == ["UPDATE 1"]
              and one(c, "SELECT d FROM m WHERE id = 2") == "2027-01-01")
        c.close()
    finally:
        srv.cleanup()


def t_extended_params():
    print("== extended protocol params with new types ==")
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        # Parse/Bind/Execute with a text-format $1 bound as unknown type,
        # inserted into a DATE column (uses pin_param + input function).
        stmt = b"S1\x00SELECT $1::date\x00\x00\x00"
        # Parse: "S1", query, no param OIDs.
        parse = b"P" + struct.pack("!i", 4 + len(b"S1\x00")
                                  + len(b"SELECT $1::date\x00") + 2) \
            + b"S1\x00SELECT $1::date\x00" + struct.pack("!h", 0)
        # Bind: portal "", stmt "S1", text params ["2026-03-04"], text results.
        val = b"2026-03-04"
        bind = b"B" + struct.pack("!i", 4 + 1 + 3 + 2 + 2 + 4 + len(val) + 2) \
            + b"\x00S1\x00" + struct.pack("!h", 0) \
            + struct.pack("!h", 1) + struct.pack("!i", len(val)) + val \
            + struct.pack("!h", 0)
        # Describe: 'S' (statement) + name cstring.
        desc = b"D" + struct.pack("!i", 4 + 1 + 3) + b"S" + b"S1\x00"
        # Execute: portal "" (cstring) + row limit int32.
        exe = b"E" + struct.pack("!i", 4 + 1 + 4) + b"\x00" + struct.pack("!i", 0)
        sync = b"S" + struct.pack("!i", 4)
        c.s.sendall(parse + bind + desc + exe + sync)
        rows = []
        while True:
            t, p = c._read_msg()
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    r.append(None if ln == -1 else p[pos:pos + ln].decode())
                    pos += ln if ln != -1 else 0
                rows.append(r)
            elif t == b"Z":
                break
        check("param $1::date via extended protocol",
              rows == [["2026-03-04"]], str(rows))
        c.close()
    finally:
        srv.cleanup()


def main():
    if not os.path.exists(BIN):
        print(f"build the server first: cargo build (missing {BIN})")
        sys.exit(2)
    t_type_roundtrip()
    t_casts()
    t_promotion()
    t_bool_logic()
    t_functions()
    t_datetime_arith()
    t_operators()
    t_null_ordering()
    t_agg_nulls()
    t_wal_new_types()
    t_old_data_dir_refused()
    t_mvcc_new_types()
    t_extended_params()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
