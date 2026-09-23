#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.21 (float8 conformance burn-down).

Groups:
  A. float8 math functions -- trunc, round, ceil, ceiling, floor, sign,
     power, exp, ln, log, sqrt, cbrt, sinh, cosh, tanh, asin, acos,
     atan, atan2, lgamma, erf, erfc. Verified over the wire with PG
     semantics (2201E for ln(<=0), 22003 for overflow, 2201F for
     sqrt(negative), etc).
  B. Prefix operators -- @ (abs), |/ (sqrt), ||/ (cbrt) for float8.
  C. Infinity/NaN arithmetic -- 'Infinity'+100 = Infinity (not 22003),
     'nan'/'0' = NaN (not 22012), 42/'Infinity' = 0 (not 22003).
  D. Underflow on input -- '10e-400'::float8 must ERROR.
  E. float8send -- 8-byte big-endian IEEE 754 as bytea.
  F. VALUES column aliases -- FROM (VALUES ...) AS t(x, y).
  G. Implicit unknown-literal coercion -- f.f1 = '1004.3' works;
     SELECT 1 + 'abc' -> 22P02 (PG coerces then fails conversion).
  H. float4 parity spot checks.
  I. v0.22 UPDATE/DELETE -- qualified target refs (t.f1), full boolean
     WHERE predicates (AND/OR/comparisons), not just col = literal.
  J. v0.22 float ordering -- Infinity comparisons, ORDER BY with infinities.
  K. v0.22 float8send byte-exact -- 8-byte big-endian IEEE 754 verified
     against struct.pack('>d').
  L. v0.22 wide numerics -- round()/trunc() of 1e200 / 1e-200, numeric
     literals beyond i128, negative round scales.
  M. v0.22 TEMP tables -- shadowing/reveal, cross-session isolation,
     disconnect cleanup, rollback, permanent indexes surviving a
     same-named TEMP shadow, TEMP UNIQUE + ON CONFLICT.
  N. v0.22 bounded CREATE TYPE -- shell, duplicate 42710, LIKE completion,
     unknown base 42704, DROP TYPE / IF EXISTS, transactional rollback.
  O. v0.22 numeric comparisons at negative scale -- =, >, <, floor, ceil,
     abs on 1e200-magnitude numerics.

Boots its own server on 127.0.0.1:5433 with a scratch data dir.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test21.py`.
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


def read_msg(s):
    hdr = _read_exact(s, 5)
    typ = hdr[0:1]
    ln = struct.unpack("!i", hdr[1:5])[0]
    payload = _read_exact(s, ln - 4) if ln > 4 else b""
    return typ, payload


def parse_datarow(payload):
    n = struct.unpack("!h", payload[0:2])[0]
    pos = 2
    out = []
    for _ in range(n):
        ln = struct.unpack("!i", payload[pos:pos + 4])[0]
        pos += 4
        if ln == -1:
            out.append(None)
        else:
            out.append(payload[pos:pos + ln].decode())
            pos += ln
    return out


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        params = cstr("user") + cstr("postgres") + cstr("database") + cstr("postgres") + b"\x00"
        self.s.sendall(struct.pack("!i", len(params) + 8) + struct.pack("!i", 196608) + params)
        while True:
            typ, _ = read_msg(self.s)
            if typ == b"Z":
                break

    def sql(self, q):
        """Run simple-protocol query; return (rows, colnames, errcode)."""
        self.s.sendall(msg(b"Q", cstr(q)))
        rows, colnames, err = [], [], None
        while True:
            typ, payload = read_msg(self.s)
            if typ == b"T":
                n = struct.unpack("!h", payload[0:2])[0]
                pos = 2
                for _ in range(n):
                    j = payload.index(b"\x00", pos)
                    colnames.append(payload[pos:j].decode())
                    pos = j + 1
                    pos += 4 + 2 + 4 + 2 + 4 + 2
            elif typ == b"D":
                rows.append(parse_datarow(payload))
            elif typ == b"E":
                err = err_code(payload)
            elif typ == b"Z":
                break
        return rows, colnames, err

    def close(self):
        self.s.close()


def one(c, q):
    rows, _, err = c.sql(q)
    if err:
        return f"ERR:{err}"
    return rows[0][0] if rows and rows[0] else None


def errcode(c, q):
    _, _, err = c.sql(q)
    return err or ""


def main():
    tmp = tempfile.mkdtemp(prefix="rg21_")
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", tmp],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(100):
            try:
                s = socket.create_connection((HOST, PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.1)
        c = Conn()

        # --- A. float8 math functions ---
        check("trunc", one(c, "SELECT trunc(1.9::float8)") == "1")
        check("trunc neg", one(c, "SELECT trunc(-1.9::float8)") == "-1")
        # v0.67: PG19 round(float8) -> float8 via C rint (half to
        # even) — round(2.5::float8) is 2, not 3. The old "3" encoded
        # the buggy numeric-detour path.
        check("round", one(c, "SELECT round(2.5::float8)") == "2"
              and one(c, "SELECT round(2.4::float8)") == "2"
              and one(c, "SELECT round(3.5::float8)") == "4")
        check("ceil", one(c, "SELECT ceil(2.1::float8)") == "3")
        check("ceiling", one(c, "SELECT ceiling(2.1::float8)") == "3")
        check("floor", one(c, "SELECT floor(2.9::float8)") == "2")
        check("sign pos", one(c, "SELECT sign(2.5::float8)") == "1")
        check("sign neg", one(c, "SELECT sign(-2.5::float8)") == "-1")
        check("sign zero", one(c, "SELECT sign(0.0::float8)") == "0")
        check("power", one(c, "SELECT power(2::float8, 10::float8)") == "1024")
        check("power zero-neginf -> err",
              errcode(c, "SELECT power(0::float8, '-Infinity'::float8)") != "")
        check("power neginf-frac -> err",
              errcode(c, "SELECT power('-Infinity'::float8, 3.5::float8)") != "")
        check("exp", one(c, "SELECT exp(0::float8)") == "1")
        check("exp overflow -> 22003",
              errcode(c, "SELECT exp(1000::float8)") == "22003")
        check("ln", abs(float(one(c, "SELECT ln(2.718281828459045::float8)")) - 1.0) < 1e-12)
        check("ln(<=0) -> 2201E",
              errcode(c, "SELECT ln(0::float8)") == "2201E"
              and errcode(c, "SELECT ln(-1::float8)") == "2201E")
        check("log", one(c, "SELECT log(100::float8)") == "2")
        check("sqrt", one(c, "SELECT sqrt(64::float8)") == "8")
        check("sqrt negative -> 2201F",
              errcode(c, "SELECT sqrt(-1::float8)") == "2201F")
        check("cbrt", one(c, "SELECT cbrt(27::float8)") == "3")
        check("sinh", abs(float(one(c, "SELECT sinh(0::float8)"))) < 1e-15)
        check("cosh", one(c, "SELECT cosh(0::float8)") == "1")
        check("tanh", abs(float(one(c, "SELECT tanh(0::float8)"))) < 1e-15)
        check("asin", abs(float(one(c, "SELECT asin(1::float8)")) - 1.5707963267948966) < 1e-12)
        check("acos", abs(float(one(c, "SELECT acos(1::float8)"))) < 1e-12)
        check("atan", abs(float(one(c, "SELECT atan(1::float8)")) - 0.7853981633974483) < 1e-12)
        check("atan2", abs(float(one(c, "SELECT atan2(1::float8, 1::float8)")) - 0.7853981633974483) < 1e-12)
        check("lgamma", abs(float(one(c, "SELECT lgamma(5::float8)")) - 3.1780538303479458) < 1e-9)
        check("lgamma(1)=0", abs(float(one(c, "SELECT lgamma(1::float8)"))) < 1e-9)
        check("erf(0)=0", abs(float(one(c, "SELECT erf(0::float8)"))) < 1e-15)
        check("erfc(0)=1", one(c, "SELECT erfc(0::float8)") == "1")
        check("erf+erfc=1",
              abs(float(one(c, "SELECT erf(0.1::float8)")) + float(one(c, "SELECT erfc(0.1::float8)")) - 1.0) < 1e-12)

        # --- B. prefix operators ---
        check("@ abs float8", one(c, "SELECT @ (-2.5::float8)") == "2.5")
        check("@ abs int", one(c, "SELECT @ (-5)") == "5")
        check("|/ sqrt", one(c, "SELECT |/ 64::float8") == "8")
        check("||/ cbrt", one(c, "SELECT ||/ 27::float8") == "3")

        # --- C. Infinity/NaN arithmetic ---
        check("inf + 100", one(c, "SELECT 'Infinity'::float8 + 100.0") == "Infinity")
        check("nan / 0", one(c, "SELECT 'nan'::float8 / '0'::float8") == "NaN")
        check("42 / inf", one(c, "SELECT 42::float8 / 'Infinity'::float8") == "0")
        check("inf * 0 -> nan", one(c, "SELECT 'Infinity'::float8 * 0::float8") == "NaN")
        check("1/0 -> 22012", errcode(c, "SELECT 1::float8 / 0::float8") == "22012")
        check("finite overflow -> 22003",
              errcode(c, "SELECT 1e308::float8 * 10::float8") == "22003")

        # --- D. underflow on input ---
        check("10e-400 errors", errcode(c, "SELECT '10e-400'::float8") != "")
        check("-10e-400 errors", errcode(c, "SELECT '-10e-400'::float8") != "")

        # --- E. float8send ---
        check("float8send", one(c, "SELECT float8send(1.0::float8)") is not None
              and errcode(c, "SELECT float8send(1.0::float8)") == "")

        # --- F. VALUES column aliases ---
        rows, cols, err = c.sql("SELECT x, y FROM (VALUES (1, 2), (3, 4)) AS t(x, y) ORDER BY x")
        check("values col aliases", err in ("", None) and cols == ["x", "y"]
              and rows == [["1", "2"], ["3", "4"]], f"err={err} cols={cols} rows={rows}")

        # --- G. implicit unknown-literal coercion ---
        c.sql("CREATE TABLE f21tbl(f1 float8)")
        c.sql("INSERT INTO f21tbl VALUES (1004.3), (200.5)")
        check("float = text literal",
              one(c, "SELECT count(*) FROM f21tbl WHERE f1 = '1004.3'") == "1")
        check("float < text literal",
              one(c, "SELECT count(*) FROM f21tbl WHERE f1 < '1004.3'") == "1")
        check("float + text literal",
              one(c, "SELECT f1 + '0.7' FROM f21tbl WHERE f1 = '1004.3'") == "1005")
        check("1 + 'abc' -> 22P02", errcode(c, "SELECT 1 + 'abc'") == "22P02")
        c.sql("DROP TABLE f21tbl")

        # --- H. float4 parity spot checks ---
        check("float4 sqrt", one(c, "SELECT sqrt(16::float4)") == "4")
        check("float4 abs prefix", one(c, "SELECT @ (-3::float4)") == "3")
        check("float4 inf arithmetic",
              one(c, "SELECT 'Infinity'::float4 + 1::float4") == "Infinity")

        # --- I. v0.22: qualified UPDATE + full WHERE predicates ---
        c.sql("CREATE TABLE f22u(id int, f1 float8)")
        c.sql("INSERT INTO f22u VALUES (1, 1004.3), (2, -34.84), (3, 0.5)")
        c.sql("UPDATE f22u SET f1 = f22u.f1 * '-1' WHERE f22u.f1 > '0.0'")
        rows, _, err = c.sql("SELECT id, f1 FROM f22u ORDER BY id")
        check("upd qualified arith", err in ("", None)
              and rows == [["1", "-1004.3"], ["2", "-34.84"], ["3", "-0.5"]],
              f"err={err} rows={rows}")
        c.sql("UPDATE f22u SET f1 = 0 WHERE id = 1 AND f1 < 0")
        check("upd AND predicate", one(c, "SELECT f1 FROM f22u WHERE id = 1") == "0")
        c.sql("UPDATE f22u SET f1 = 7.5 WHERE id = 2 OR id = 3")
        check("upd OR predicate",
              one(c, "SELECT count(*) FROM f22u WHERE f1 = 7.5") == "2")
        c.sql("DELETE FROM f22u WHERE f22u.id = 1")
        check("del qualified", one(c, "SELECT count(*) FROM f22u") == "2")
        c.sql("DELETE FROM f22u WHERE f1 > 7 OR f1 < 1")
        check("del OR predicate", one(c, "SELECT count(*) FROM f22u") == "0")
        c.sql("DROP TABLE f22u")

        # --- J. v0.22: float ordering / Infinity comparisons ---
        check("inf > 1e308",
              one(c, "SELECT 'Infinity'::float8 > 1e308::float8") == "t")
        check("-inf < -1e308",
              one(c, "SELECT '-Infinity'::float8 < -1e308::float8") == "t")
        c.sql("CREATE TABLE f22o(f float8)")
        c.sql("INSERT INTO f22o VALUES (1.5), ('-Infinity'), ('Infinity'), (-2.5)")
        rows, _, err = c.sql("SELECT f FROM f22o ORDER BY f")
        check("order with inf", err in ("", None)
              and rows == [["-Infinity"], ["-2.5"], ["1.5"], ["Infinity"]],
              f"err={err} rows={rows}")
        c.sql("DROP TABLE f22o")

        # --- K. v0.22: float8send byte-exact ---
        want = "\\x" + struct.pack(">d", 1.0).hex()
        check("float8send bytes 1.0",
              one(c, "SELECT float8send(1.0::float8)") == want)
        want0 = "\\x" + struct.pack(">d", 0.0).hex()
        check("float8send bytes 0.0",
              one(c, "SELECT float8send(0.0::float8)") == want0)

        # --- L. v0.22: wide numerics (beyond i128) ---
        check("round 1e200 float8",
              one(c, "SELECT round('1.2345678901234e200'::float8)")
              == "1.2345678901234e+200")
        check("round -1e200 float8",
              one(c, "SELECT round('-1.2345678901234e200'::float8)")
              == "-1.2345678901234e+200")
        check("round 1e-200 float8",
              one(c, "SELECT round('1.2345678901234e-200'::float8)") == "0")
        # v0.67: PG19 numeric_out prints plain decimal (get_str_from_var),
        # never scientific — the old "1e+200" expectations were stale.
        check("1e200 numeric literal",
              one(c, "SELECT '1e200'::numeric") == "1" + "0" * 200)
        check("round 1e200 numeric",
              one(c, "SELECT round('1e200'::numeric)") == "1" + "0" * 200)
        check("round(12345,-1)",
              one(c, "SELECT round(12345::numeric, -1)") == "12350")
        check("round(12345,-2)",
              one(c, "SELECT round(12345::numeric, -2)") == "12300")
        check("trunc 1e200 numeric",
              one(c, "SELECT trunc('1e200'::numeric)") == "1" + "0" * 200)

        # --- M. v0.22: TEMP table semantics ---
        c.sql("CREATE TABLE t22perm(id int)")
        c.sql("INSERT INTO t22perm VALUES (1), (2)")
        c.sql("CREATE TEMP TABLE t22perm(id int)")
        c.sql("INSERT INTO t22perm VALUES (10)")
        check("temp shadows perm",
              one(c, "SELECT count(*) FROM t22perm") == "1"
              and one(c, "SELECT sum(id) FROM t22perm") == "10")
        c.sql("DROP TABLE t22perm")
        check("perm revealed", one(c, "SELECT sum(id) FROM t22perm") == "3")
        c.sql("CREATE TEMP TABLE t22s1(id int)")
        c.sql("INSERT INTO t22s1 VALUES (1)")
        c2 = Conn()
        check("temp cross-session hidden",
              one(c2, "SELECT count(*) FROM t22s1").startswith("ERR:"))
        c2.sql("CREATE TEMP TABLE t22disc(id int)")
        c2.sql("INSERT INTO t22disc VALUES (9)")
        c2.close()
        c3 = Conn()
        check("temp disconnect cleanup",
              errcode(c3, "CREATE TEMP TABLE t22disc(id int)") == ""
              and one(c3, "SELECT count(*) FROM t22disc") == "0")
        c3.close()
        c.sql("BEGIN")
        c.sql("CREATE TEMP TABLE t22rb(id int)")
        c.sql("INSERT INTO t22rb VALUES (5)")
        c.sql("ROLLBACK")
        check("temp rollback",
              one(c, "SELECT count(*) FROM t22rb").startswith("ERR:"))
        c.sql("CREATE TABLE t22pi(id int PRIMARY KEY)")
        c.sql("INSERT INTO t22pi VALUES (1), (2)")
        c.sql("CREATE TEMP TABLE t22pi(id int)")
        c.sql("INSERT INTO t22pi VALUES (100)")
        c.sql("DROP TABLE t22pi")
        check("perm index survives",
              one(c, "SELECT sum(id) FROM t22pi") == "3"
              and errcode(c, "INSERT INTO t22pi VALUES (1)") == "23505")
        c.sql("DROP TABLE t22pi")
        c.sql("CREATE TEMP TABLE t22c(id int UNIQUE)")
        c.sql("INSERT INTO t22c VALUES (1)")
        check("temp unique enforced",
              errcode(c, "INSERT INTO t22c VALUES (1)") == "23505")
        c.sql("INSERT INTO t22c VALUES (1) ON CONFLICT DO NOTHING")
        check("temp on conflict",
              one(c, "SELECT count(*) FROM t22c") == "1")
        c.sql("DROP TABLE t22c")
        c.sql("DROP TABLE t22s1")
        c.sql("DROP TABLE t22perm")

        # --- N. v0.22: bounded CREATE TYPE ---
        check("create type shell", errcode(c, "CREATE TYPE t22shell;") == "")
        check("create type dup -> 42710",
              errcode(c, "CREATE TYPE t22shell;") == "42710")
        check("create type like bad -> 42704",
              errcode(c, "CREATE TYPE t22bad (input = f, output = g,"
                         " like = no_such_type);") == "42704")
        check("create type like float8",
              errcode(c, "CREATE TYPE t22shell (input = f, output = g,"
                         " like = float8);") == "")
        check("create type direct",
              errcode(c, "CREATE TYPE t22direct (input = f, output = g,"
                         " like = bigint);") == "")
        c.sql("DROP TYPE t22direct")
        check("drop type", errcode(c, "DROP TYPE t22shell;") == "")
        check("drop type missing -> 42704",
              errcode(c, "DROP TYPE t22nosuch;") == "42704")
        check("drop type if exists",
              errcode(c, "DROP TYPE IF EXISTS t22nosuch;") == "")
        c.sql("BEGIN")
        c.sql("CREATE TYPE t22typerb;")
        c.sql("ROLLBACK")
        check("create type rollback",
              errcode(c, "CREATE TYPE t22typerb;") == "")
        c.sql("DROP TYPE t22typerb")

        # --- O. v0.22: numeric comparisons at negative scale ---
        check("1e200 = 1e200",
              one(c, "SELECT '1e200'::numeric = '1e200'::numeric") == "t")
        check("2e200 > 1e200",
              one(c, "SELECT '2e200'::numeric > '1e200'::numeric") == "t")
        check("-1e200 < 1e200",
              one(c, "SELECT '-1e200'::numeric < '1e200'::numeric") == "t")
        check("floor 1e-200",
              one(c, "SELECT floor('1e-200'::numeric)") == "0")
        check("ceil 1e-200",
              one(c, "SELECT ceil('1e-200'::numeric)") == "1")
        check("abs -1e200",
              one(c, "SELECT abs('-1e200'::numeric)") == "1" + "0" * 200)

        c.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    print(f"\n{len(passed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
