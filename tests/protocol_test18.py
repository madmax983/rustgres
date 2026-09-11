#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.18 (numeric conformance burn-down).

Groups:
  A. NaN/Infinity parsing -- case-insensitive 'NaN', 'Inf', 'Infinity',
     signed variants; INSERT with NaN no longer aborts transaction
     (the v0.17 423-error 25P02 cascade fix).
  B. Special-value arithmetic -- NaN/Inf propagation in +,-,*,/, comparisons,
     ordering (-Inf < finite < Inf < NaN), NaN != NaN.
  C. exp/ln/log -- high-precision (~15 digits), special values, SQLSTATEs
     (2201E for ln(<=0), 22003 for overflow).
  D. Power with specials -- power('inf','-2')=0, etc.
  E. Column naming -- SELECT sqrt(2) names column "sqrt", not "?column?".
  F. v0.18-repair -- every numeric function that previously died in
     func_result_type (42883) or panicked on zero args: cbrt, factorial,
     gcd/lcm, pi, degrees/radians, scale/min_scale/trim_scale, div,
     width_bucket, random/setseed. Verified over the wire.

Boots its own server on 127.0.0.1:5433 with a scratch data dir.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test18.py`.
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


def main():
    tmp = tempfile.mkdtemp(prefix="rg18_")
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", tmp],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(1.5)
    try:
        c = Conn()

        # ---- A. NaN/Infinity parsing (case-insensitive) ----
        for lit, expect in [
            ("'NaN'", "NaN"), ("'nan'", "NaN"), ("'NAN'", "NaN"), ("'nAn'", "NaN"),
            ("'-NaN'", "NaN"), ("'+NaN'", "NaN"),
            ("'Inf'", "Infinity"), ("'inf'", "Infinity"), ("'INF'", "Infinity"),
            ("'Infinity'", "Infinity"), ("'infinity'", "Infinity"), ("'INFINITY'", "Infinity"),
            ("'-Infinity'", "-Infinity"), ("'-inf'", "-Infinity"), ("'-Infinity'", "-Infinity"),
            ("'+Inf'", "Infinity"), ("'+Infinity'", "Infinity"),
        ]:
            rows, _, err = c.sql(f"SELECT {lit}::numeric")
            check(f"parse {lit}", err is None and rows and rows[0][0] == expect,
                  f"got {rows} err={err}")

        # The v0.17 cascade: INSERT with NaN in explicit txn must not abort.
        c.sql("CREATE TABLE num_exp_div(a numeric, b numeric, c numeric)")
        rows, _, err = c.sql("BEGIN")
        check("txn begin", err is None, f"err={err}")
        rows, _, err = c.sql("INSERT INTO num_exp_div VALUES (0,0,'NaN')")
        check("insert NaN (no 22P02)", err is None, f"err={err}")
        rows, _, err = c.sql("INSERT INTO num_exp_div VALUES (1,2,'3.5')")
        check("insert after NaN (no 25P02 cascade)", err is None, f"err={err}")
        rows, _, err = c.sql("INSERT INTO num_exp_div VALUES (2,3,'Infinity')")
        check("insert Infinity", err is None, f"err={err}")
        rows, _, err = c.sql("COMMIT")
        check("txn commit", err is None, f"err={err}")
        rows, _, err = c.sql("SELECT c FROM num_exp_div ORDER BY a")
        check("NaN roundtrip", err is None and len(rows) == 3 and rows[0][0] == "NaN",
              f"got {rows}")

        # ---- B. Special-value arithmetic ----
        for sql, expect in [
            ("SELECT 'NaN'::numeric + 1", "NaN"),
            ("SELECT 1 + 'NaN'::numeric", "NaN"),
            ("SELECT 'NaN'::numeric - 1", "NaN"),
            ("SELECT 'NaN'::numeric * 1", "NaN"),
            ("SELECT 'NaN'::numeric / 1", "NaN"),
            ("SELECT 'Infinity'::numeric + 1", "Infinity"),
            ("SELECT 'Infinity'::numeric - 1", "Infinity"),
            ("SELECT '-Infinity'::numeric + 1", "-Infinity"),
            ("SELECT 'Infinity'::numeric + '-Infinity'::numeric", "NaN"),
            ("SELECT 'Infinity'::numeric * 2", "Infinity"),
            ("SELECT 'Infinity'::numeric * 0", "NaN"),
            ("SELECT '-Infinity'::numeric * 2", "-Infinity"),
            ("SELECT '-Infinity'::numeric * -2", "Infinity"),
            ("SELECT 'Infinity'::numeric / 2", "Infinity"),
            ("SELECT 1 / 'Infinity'::numeric", "0"),
        ]:
            rows, _, _ = c.sql(sql)
            check(f"arith {sql[7:30]}", rows and rows[0][0] == expect, f"got {rows}")

        # Ordering: -Inf < finite < Inf < NaN
        rows, _, _ = c.sql(
            "SELECT x FROM (VALUES ('-Infinity'::numeric), ('1'::numeric), "
            "('Infinity'::numeric), ('NaN'::numeric)) v(x) ORDER BY x")
        if rows:
            check("special ordering", [r[0] for r in rows] == ["-Infinity", "1", "Infinity", "NaN"],
                  f"got {rows}")
        # NaN != NaN
        rows, _, _ = c.sql("SELECT 'NaN'::numeric = 'NaN'::numeric")
        check("NaN != NaN", rows[0][0] == "f", f"got {rows}")
        rows, _, _ = c.sql("SELECT 'NaN'::numeric <> 'NaN'::numeric")
        check("NaN <> NaN", rows[0][0] == "t", f"got {rows}")
        # Comparisons with Inf
        rows, _, _ = c.sql("SELECT 'Infinity'::numeric > 1000000::numeric")
        check("Inf > 1e6", rows[0][0] == "t", f"got {rows}")
        rows, _, _ = c.sql("SELECT '-Infinity'::numeric < -1000000::numeric")
        check("-Inf < -1e6", rows[0][0] == "t", f"got {rows}")

        # ---- C. exp/ln/log ----
        rows, _, _ = c.sql("SELECT exp(1.0)")
        check("exp(1.0)", rows[0][0].startswith("2.718281828459045"), f"got {rows}")
        rows, _, _ = c.sql("SELECT exp(0::numeric)")
        check("exp(0)=1", rows[0][0].startswith("1"), f"got {rows}")
        rows, _, _ = c.sql("SELECT exp('NaN'::numeric)")
        check("exp(NaN)=NaN", rows[0][0] == "NaN", f"got {rows}")
        rows, _, _ = c.sql("SELECT exp('Infinity'::numeric)")
        check("exp(Inf)=Inf", rows[0][0] == "Infinity", f"got {rows}")
        rows, _, _ = c.sql("SELECT exp('-Infinity'::numeric)")
        check("exp(-Inf)=0", rows[0][0] == "0", f"got {rows}")
        rows, _, err = c.sql("SELECT exp(1000::numeric)")
        check("exp overflow 22003", err == "22003", f"err={err} rows={rows}")
        rows, _, _ = c.sql("SELECT ln(1::numeric)")
        check("ln(1)=0", rows[0][0].startswith("0"), f"got {rows}")
        rows, _, _ = c.sql("SELECT ln(4.2::numeric)")
        check("ln(4.2)", rows[0][0].startswith("1.435084525289322"), f"got {rows}")
        rows, _, _ = c.sql("SELECT ln('NaN'::numeric)")
        check("ln(NaN)=NaN", rows[0][0] == "NaN", f"got {rows}")
        rows, _, _ = c.sql("SELECT ln('Infinity'::numeric)")
        check("ln(Inf)=Inf", rows[0][0] == "Infinity", f"got {rows}")
        rows, _, err = c.sql("SELECT ln(0::numeric)")
        check("ln(0) 2201E", err == "2201E", f"err={err}")
        rows, _, err = c.sql("SELECT ln(-1::numeric)")
        check("ln(-1) 2201E", err == "2201E", f"err={err}")
        rows, _, _ = c.sql("SELECT log(10::numeric)")
        check("log(10)=1", rows[0][0].startswith("1"), f"got {rows}")
        rows, _, _ = c.sql("SELECT log(4.2::numeric)")
        check("log(4.2)", rows[0][0].startswith("0.62324929039790"), f"got {rows}")
        rows, _, _ = c.sql("SELECT log(2::numeric, 8::numeric)")
        check("log(2,8)=3", rows[0][0].startswith("3"), f"got {rows}")
        rows, _, _ = c.sql("SELECT log(10::numeric, 100::numeric)")
        check("log(10,100)=2", rows[0][0].startswith("2"), f"got {rows}")

        # ---- D. Power with specials ----
        rows, _, _ = c.sql("SELECT power('Infinity'::numeric, '-2'::numeric)")
        check("power(Inf,-2)=0", rows[0][0] == "0", f"got {rows}")
        rows, _, _ = c.sql("SELECT power('-Infinity'::numeric, '3'::numeric)")
        check("power(-Inf,3)=-Inf", rows[0][0] == "-Infinity", f"got {rows}")
        rows, _, err = c.sql("SELECT power('-Infinity'::numeric, '4.5'::numeric)")
        check("power(-Inf,4.5) errors", err is not None, f"err={err}")
        rows, _, err = c.sql("SELECT power('0'::numeric, '-1'::numeric)")
        check("power(0,-1) errors", err is not None, f"err={err}")

        # ---- E. Column naming ----
        rows, colnames, _ = c.sql("SELECT sqrt(2)")
        check("sqrt colname", colnames and colnames[0] == "sqrt", f"got {colnames}")
        rows, colnames, _ = c.sql("SELECT exp(1.0)")
        check("exp colname", colnames and colnames[0] == "exp", f"got {colnames}")
        rows, colnames, _ = c.sql("SELECT ln(2::numeric)")
        check("ln colname", colnames and colnames[0] == "ln", f"got {colnames}")
        rows, colnames, _ = c.sql("SELECT 1 + 1")
        check("expr colname ?column?", colnames and colnames[0] == "?column?",
              f"got {colnames}")
        rows, colnames, _ = c.sql("SELECT 1 + 1 AS sum")
        check("alias colname", colnames and colnames[0] == "sum", f"got {colnames}")

        # ---- F. Additional edge cases ----
        # Decimal literals are numeric (v0.18 change)
        rows, _, _ = c.sql("SELECT 1.5 + 2.5")
        check("decimal literal numeric", rows[0][0] == "4", f"got {rows}")
        # NaN in different contexts
        rows, _, _ = c.sql("SELECT 'nan'::numeric + 'inf'::numeric")
        check("nan+inf=NaN", rows[0][0] == "NaN", f"got {rows}")
        # Infinity arithmetic
        rows, _, _ = c.sql("SELECT 'Infinity'::numeric - 'Infinity'::numeric")
        check("Inf-Inf=NaN", rows[0][0] == "NaN", f"got {rows}")
        # exp with integer
        rows, _, _ = c.sql("SELECT exp(2)")
        check("exp(2)", rows[0][0].startswith("7.38905609893"), f"got {rows}")
        # ln with integer
        rows, _, _ = c.sql("SELECT ln(10)")
        check("ln(10)", rows[0][0].startswith("2.30258509299"), f"got {rows}")
        # log base 10
        rows, _, _ = c.sql("SELECT log(1000::numeric)")
        check("log(1000)=3", rows[0][0].startswith("3"), f"got {rows}")
        # abs with specials
        rows, _, _ = c.sql("SELECT abs('-Infinity'::numeric)")
        check("abs(-Inf)=Inf", rows[0][0] == "Infinity", f"got {rows}")
        rows, _, _ = c.sql("SELECT abs('NaN'::numeric)")
        check("abs(NaN)=NaN", rows[0][0] == "NaN", f"got {rows}")
        # sign with specials
        rows, _, _ = c.sql("SELECT sign('Infinity'::numeric)")
        check("sign(Inf)=1", rows[0][0] == "1", f"got {rows}")
        rows, _, _ = c.sql("SELECT sign('-Infinity'::numeric)")
        check("sign(-Inf)=-1", rows[0][0] == "-1", f"got {rows}")
        rows, _, _ = c.sql("SELECT sign('NaN'::numeric)")
        check("sign(NaN)=NaN", rows[0][0] == "NaN", f"got {rows}")
        # sqrt with specials
        rows, _, _ = c.sql("SELECT sqrt('Infinity'::numeric)")
        check("sqrt(Inf)=Inf", rows[0][0] == "Infinity", f"got {rows}")
        rows, _, _ = c.sql("SELECT sqrt('NaN'::numeric)")
        check("sqrt(NaN)=NaN", rows[0][0] == "NaN", f"got {rows}")
        # floor/ceil with specials
        rows, _, _ = c.sql("SELECT floor('Infinity'::numeric)")
        check("floor(Inf)=Inf", rows[0][0] == "Infinity", f"got {rows}")
        rows, _, _ = c.sql("SELECT ceil('-Infinity'::numeric)")
        check("ceil(-Inf)=-Inf", rows[0][0] == "-Infinity", f"got {rows}")
        # round with specials
        rows, _, _ = c.sql("SELECT round('NaN'::numeric)")
        check("round(NaN)=NaN", rows[0][0] == "NaN", f"got {rows}")
        # power edge cases
        rows, _, _ = c.sql("SELECT power('Infinity'::numeric, '0'::numeric)")
        check("power(Inf,0)=1", rows[0][0] == "1", f"got {rows}")
        rows, _, _ = c.sql("SELECT power('-1'::numeric, 'Infinity'::numeric)")
        check("power(-1,Inf)=1", rows[0][0] == "1", f"got {rows}")
        rows, _, _ = c.sql("SELECT power('-2'::numeric, 'Infinity'::numeric)")
        check("power(-2,Inf)=Inf", rows[0][0] == "Infinity", f"got {rows}")
        # mod with specials
        rows, _, _ = c.sql("SELECT mod('NaN'::numeric, 2::numeric)")
        check("mod(NaN,2)=NaN", rows[0][0] == "NaN", f"got {rows}")
        # Comparison chain
        rows, _, _ = c.sql("SELECT '-Infinity'::numeric < 'Infinity'::numeric")
        check("-Inf < Inf", rows[0][0] == "t", f"got {rows}")
        rows, _, _ = c.sql("SELECT 'NaN'::numeric > 'Infinity'::numeric")
        check("NaN > Inf (ordering)", rows[0][0] == "t", f"got {rows}")
        # exp/ln roundtrip (precision: ~15 digits, not exact)
        rows, _, _ = c.sql("SELECT ln(exp(1::numeric))")
        check("ln(exp(1))~1", rows[0][0].startswith("0.9999999999999999") or rows[0][0].startswith("1"),
              f"got {rows}")

        # Group F: v0.18-repair — functions that previously died in
        # func_result_type (42883) or panicked on zero args. All verified
        # over the wire here.
        rows, names, err = c.sql("SELECT cbrt(8)")
        check("cbrt(8)=2", err is None and rows[0][0] == "2", f"got {rows} err={err}")
        check("cbrt colname", names == ["cbrt"], f"got {names}")
        rows, _, _ = c.sql("SELECT cbrt(-27)")
        check("cbrt(-27)=-3", rows[0][0] == "-3", f"got {rows}")
        rows, names, _ = c.sql("SELECT factorial(5)")
        check("factorial(5)=120", rows[0][0] == "120", f"got {rows}")
        check("factorial colname", names == ["factorial"], f"got {names}")
        rows, _, _ = c.sql("SELECT factorial(0)")
        check("factorial(0)=1", rows[0][0] == "1", f"got {rows}")
        rows, _, err = c.sql("SELECT factorial(-1)")
        check("factorial(-1) 2201F", err == "2201F", f"got err={err}")
        rows, names, _ = c.sql("SELECT gcd(12, 18)")
        check("gcd(12,18)=6", rows[0][0] == "6", f"got {rows}")
        check("gcd colname", names == ["gcd"], f"got {names}")
        rows, _, _ = c.sql("SELECT lcm(4, 6)")
        check("lcm(4,6)=12", rows[0][0] == "12", f"got {rows}")
        rows, names, err = c.sql("SELECT pi()")
        check("pi() dispatches", err is None and rows[0][0].startswith("3.14159265358979"),
              f"got {rows} err={err}")
        check("pi colname", names == ["pi"], f"got {names}")
        rows, _, err = c.sql("SELECT degrees(1)")
        check("degrees(1)~57.29", err is None and rows[0][0].startswith("57.2957795"),
              f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT radians(180)")
        check("radians(180)~pi", err is None and rows[0][0].startswith("3.141592653"),
              f"got {rows} err={err}")
        rows, _, _ = c.sql("SELECT scale(1.50)")
        check("scale(1.50)=1", rows[0][0] == "1", f"got {rows}")
        rows, _, _ = c.sql("SELECT trim_scale(1.500)")
        check("trim_scale(1.500)=1.5", rows[0][0] == "1.5", f"got {rows}")
        rows, _, _ = c.sql("SELECT min_scale(1.50)")
        check("min_scale(1.50)=1", rows[0][0] == "1", f"got {rows}")
        rows, names, _ = c.sql("SELECT div(7, 2)")
        check("div(7,2)=3", rows[0][0] == "3", f"got {rows}")
        check("div colname", names == ["div"], f"got {names}")
        rows, _, _ = c.sql("SELECT div(7, -2)")
        check("div(7,-2)=-3", rows[0][0] == "-3", f"got {rows}")
        rows, _, err = c.sql("SELECT div(7, 0)")
        check("div(7,0) 22012", err == "22012", f"got err={err}")
        rows, names, err = c.sql("SELECT width_bucket(5, 1, 10, 5)")
        check("width_bucket=3", err is None and rows[0][0] == "3", f"got {rows} err={err}")
        check("width_bucket colname", names == ["width_bucket"], f"got {names}")
        rows, _, err = c.sql("SELECT random()")
        r = float(rows[0][0]) if err is None and rows else -1.0
        check("random() in [0,1)", err is None and 0.0 <= r < 1.0, f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT setseed(0.5)")
        check("setseed(0.5) ok", err is None and rows[0][0] is None, f"got {rows} err={err}")

        # Group G: v0.18-repair — gcd/lcm panicked (i64::MIN.abs() negation
        # overflow) instead of raising 22003 like PG. Verified over the wire.
        rows, _, err = c.sql("SELECT gcd((-9223372036854775808)::int8, 0::int8)")
        check("gcd(int8min,0)=22003", err == "22003", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT gcd((-9223372036854775808)::int8, (-9223372036854775808)::int8)")
        check("gcd(int8min,int8min)=22003", err == "22003", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT lcm((-9223372036854775808)::int8, 1::int8)")
        check("lcm(int8min,1)=22003", err == "22003", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT gcd(12, 18), lcm(4, 6), gcd(0,0), lcm(0,0)")
        check("gcd/lcm basics", err is None and rows[0] == ["6", "12", "0", "0"], f"got {rows} err={err}")

        # Group H: v0.18-repair — float8 ln/log returned -Inf for log(0)
        # instead of PG's 2201E; min_scale('NaN') returned 0 not NULL.
        rows, _, err = c.sql("SELECT log(0.0::float8)")
        check("log(0.0::float8)=2201E", err == "2201E", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT ln(0.0::float8)")
        check("ln(0.0::float8)=2201E", err == "2201E", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT log(-1.0::float8)")
        check("log(-1::float8)=2201E", err == "2201E", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT log(10.0::float8)")
        check("log(10::float8)=1", err is None and rows[0][0] == "1", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT min_scale('NaN'::numeric) IS NULL")
        check("min_scale(NaN) is null", err is None and rows[0][0] == "t", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT min_scale('Infinity'::numeric) IS NULL")
        check("min_scale(Inf) is null", err is None and rows[0][0] == "t", f"got {rows} err={err}")
        rows, _, err = c.sql("SELECT min_scale(0.00)")
        check("min_scale(0.00)=0", err is None and rows[0][0] == "0", f"got {rows} err={err}")

        c.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)
        shutil.rmtree(tmp, ignore_errors=True)

    print(f"\n{len(passed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
