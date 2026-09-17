#!/usr/bin/env python3
"""protocol_test43.py — v0.43 pg_input_error_info(text, text).

v0.43 implements PG19's `pg_input_error_info(input, type)` (src/backend/
utils/adt/misc.c, REL_19_STABLE): the input function's soft error as
PG's four OUT columns (message, detail, hint, sql_error_code); a valid
input yields one row of NULLs. Ground truth is PG19 itself:
- the C source (ErrorSaveContext soft-error path),
- pg_proc.dat OUT parameter names,
- the PG regression .out files (boolean/char/varchar/int2/int4/int8/
  float4/float8/numeric/strings).

Also bundled: the v0.29/v0.35/v0.42 `pg_input_is_valid` dispatch is
refactored onto the shared `pg_input_validate` core so the two functions
cannot drift, and the table-function machinery generalizes from one text
column to N named columns.

This test manages its own server on port 5545 (so it never collides
with the conformance runner) and is RED on the v0.42 base, GREEN on the
v0.43 branch.

Sections:
  A. Shape: 4 columns named message/detail/hint/sql_error_code; valid
     input -> one row of NULLs; invalid -> PG-exact message + SQLSTATE.
  B. Per-type PG-exact errors: bool, char/varchar, int2/int4/int8,
     int2vector (element quoted, "smallint" naming), float4/float8 (no
     "value " prefix on overflow), numeric syntax/overflow,
     numeric(7,4) field-overflow detail, bytea hex errors.
  C. is_valid/error_info consistency: the shared core answers both the
     same way (including ''::int2vector, now valid like PG).
  D. FROM behaviors: table alias keeps OUT names, positional column
     aliases, 42601 on too many aliases, qualified references.
  E. Hard errors: unknown type -> 42883, malformed char typmod -> 22023,
     wrong arity -> 42883, NULL args -> zero rows.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5545
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = failed = 0


def check(name, cond):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")


# --------------------------------------------------------------------------
# Server lifecycle.
# --------------------------------------------------------------------------

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
    time.sleep(0.5)


# --------------------------------------------------------------------------
# Wire protocol.
# --------------------------------------------------------------------------

class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = self.read_msg()
            if t == b"Z":
                break

    def read_exact(self, n):
        d = b""
        while len(d) < n:
            c = self.s.recv(n - len(d))
            if not c:
                raise RuntimeError("closed")
            d += c
        return d

    def read_msg(self):
        t = self.read_exact(1)
        (ln,) = struct.unpack("!i", self.read_exact(4))
        return t, self.read_exact(ln - 4)

    def do_sql(self, q):
        self.s.sendall(b"Q" + struct.pack("!i", len(q.encode()) + 5)
                       + q.encode() + b"\x00")
        rows, cols, err, msg = [], [], None, None
        while True:
            t, p = self.read_msg()
            if t == b"T":
                (n,) = struct.unpack("!h", p[:2])
                pos = 2
                for _ in range(n):
                    e = p.index(b"\x00", pos)
                    cols.append(p[pos:e].decode())
                    pos = e + 1 + 18
            elif t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, row = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    if ln == -1:
                        row.append(None)
                    else:
                        row.append(p[pos:pos + ln].decode())
                        pos += ln
                rows.append(row)
            elif t == b"E":
                f, pos = {}, 0
                while pos < len(p) and p[pos] != 0:
                    c = chr(p[pos])
                    pos += 1
                    e = p.index(b"\x00", pos)
                    f[c] = p[pos:e].decode("utf8", "replace")
                    pos = e + 1
                err, msg = f.get("C"), f.get("M")
            elif t == b"Z":
                break
        return cols, rows, err, msg

    def close(self):
        self.s.close()


def main():
    data_dir = tempfile.mkdtemp(prefix="rg43_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        def info(inp, typ):
            cols, rows, err, msg = c.do_sql(
                f"SELECT * FROM pg_input_error_info('{inp}', '{typ}')")
            assert not err, f"info({inp!r},{typ!r}) -> {err}: {msg}"
            assert cols == ["message", "detail", "hint", "sql_error_code"], cols
            return rows

        # -- A. Shape ---------------------------------------------------
        check("A1 valid -> one row of NULLs",
              info("true", "bool") == [[None, None, None, None]])
        check("A2 invalid message/code",
              info("junk", "bool") ==
              [['invalid input syntax for type boolean: "junk"',
                None, None, "22P02"]])

        # -- B. Per-type PG-exact errors --------------------------------
        cases = [
            # (input, type, message, detail, code)
            ("abcde", "char(4)",
             "value too long for type character(4)", None, "22001"),
            ("abcde", "varchar(4)",
             "value too long for type character varying(4)", None, "22001"),
            ("50000", "int2",
             'value "50000" is out of range for type smallint', None, "22003"),
            ("xyz", "int2",
             'invalid input syntax for type smallint: "xyz"', None, "22P02"),
            ("1 asdf", "int2vector",
             'invalid input syntax for type smallint: "asdf"', None, "22P02"),
            ("50000", "int2vector",
             'value "50000" is out of range for type smallint', None, "22003"),
            ("1000000000000", "int4",
             'value "1000000000000" is out of range for type integer',
             None, "22003"),
            ("10000000000000000000", "int8",
             'value "10000000000000000000" is out of range for type bigint',
             None, "22003"),
            ("1e400", "float4",
             '"1e400" is out of range for type real', None, "22003"),
            ("1e4000", "float8",
             '"1e4000" is out of range for type double precision',
             None, "22003"),
            ("abc", "float8",
             'invalid input syntax for type double precision: "abc"',
             None, "22P02"),
            ("1e400000", "numeric",
             "value overflows numeric format", None, "22003"),
            ("0x1234.567", "numeric",
             'invalid input syntax for type numeric: "0x1234.567"',
             None, "22P02"),
            ("1234.567", "numeric(7,4)",
             "numeric field overflow",
             "A field with precision 7, scale 4 must round to an absolute "
             "value less than 10^3.", "22003"),
            # maxdigits == 0 renders the bound as "1" (PG19 numeric.c).
            ("1", "numeric(4,4)",
             "numeric field overflow",
             "A field with precision 4, scale 4 must round to an absolute "
             "value less than 1.", "22003"),
            ("Infinity", "numeric(7,2)",
             "numeric field overflow",
             "A field with precision 7, scale 2 cannot hold an infinite "
             "value.", "22003"),
        ]
        for i, (inp, typ, m, d, code) in enumerate(cases):
            rows = info(inp, typ)
            check(f"B{i} {typ} {inp!r}",
                  rows == [[m, d, None, code]])

        # bytea hex errors (E'' quoting).
        _, rows, err, _ = c.do_sql(
            r"SELECT * FROM pg_input_error_info(E'\\xDeAdBeE', 'bytea')")
        check("B-bytea-odd",
              not err and rows ==
              [["invalid hexadecimal data: odd number of digits",
                None, None, "22023"]])
        _, rows, err, _ = c.do_sql(
            r"SELECT * FROM pg_input_error_info(E'\\xDeAdBeEx', 'bytea')")
        check("B-bytea-digit",
              not err and rows ==
              [['invalid hexadecimal digit: "x"', None, None, "22023"]])
        _, rows, err, _ = c.do_sql(
            r"SELECT * FROM pg_input_error_info(E'foo\\99bar', 'bytea')")
        check("B-bytea-invalid",
              not err and rows ==
              [["invalid input syntax for type bytea", None, None, "22P02"]])

        # -- C. is_valid / error_info consistency ------------------------
        def is_valid(inp, typ):
            _, rows, err, msg = c.do_sql(
                f"SELECT pg_input_is_valid('{inp}', '{typ}')")
            assert not err, f"is_valid -> {err}: {msg}"
            return rows[0][0]

        for inp, typ, want in [
            ("true", "bool", "t"), ("asdf", "bool", "f"),
            ("50000", "int2", "f"), ("500", "int2", "t"),
            ("", "int2vector", "t"),  # PG: empty string is a valid vector
            ("1 asdf", "int2vector", "f"),
            ("1e4000", "float8", "f"), ("1.5", "float8", "t"),
            ("1e400000", "numeric", "f"),
            ("1234.567", "numeric(7,4)", "f"),
            ("1234.567", "numeric(8,4)", "t"),
            ("abcde", "char(4)", "f"), ("abcd", "char(4)", "t"),
        ]:
            got = is_valid(inp, typ)
            rows = info(inp, typ)
            agree = (got == "t") == (rows == [[None, None, None, None]])
            check(f"C {typ} {inp!r} consistent", got == want and agree)

        # -- D. FROM behaviors -------------------------------------------
        cols, rows, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info('junk', 'bool') AS e")
        check("D1 table alias keeps OUT names",
              not err and cols == ["message", "detail", "hint",
                                   "sql_error_code"])
        cols, rows, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info('junk', 'bool') AS e(m, d, h, s)")
        check("D2 positional column aliases",
              not err and cols == ["m", "d", "h", "s"]
              and rows[0][0] == 'invalid input syntax for type boolean: "junk"'
              and rows[0][3] == "22P02")
        _, _, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info('junk', 'bool') AS e(a,b,c,d,x)")
        check("D3 too many aliases -> 42601", err == "42601")
        _, rows, err, _ = c.do_sql(
            "SELECT e.sql_error_code FROM pg_input_error_info('junk', 'bool') AS e")
        check("D4 qualified reference",
              not err and rows == [["22P02"]])
        # regexp_split_to_table still single-column with alias behavior.
        cols, rows, err, _ = c.do_sql(
            "SELECT * FROM regexp_split_to_table('a,b', ',') AS s")
        check("D5 split_to_table alias",
              not err and cols == ["s"] and rows == [["a"], ["b"]])

        # -- E. Hard errors ----------------------------------------------
        _, _, err, msg = c.do_sql(
            "SELECT * FROM pg_input_error_info('x', 'bogus')")
        check("E1 unknown type -> 42883",
              err == "42883" and "pg_input_error_info" in msg)
        _, _, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info('abc', 'char(bogus)')")
        check("E2 malformed char typmod -> 22023", err == "22023")
        _, _, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info('a')")
        check("E3 wrong arity -> 42883", err == "42883")
        _, rows, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info(NULL, 'bool')")
        check("E4 NULL input -> zero rows", not err and rows == [])
        _, rows, err, _ = c.do_sql(
            "SELECT * FROM pg_input_error_info('x', 'nope_nope')")
        check("E5 unknown type names the caller", err == "42883")

        c.close()
    finally:
        stop_server(proc)

    print(f"protocol_test43: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
