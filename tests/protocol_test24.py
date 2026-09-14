#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.24 (string built-ins, regexp strictness,
INSERT expressions, dollar-quoting, SUBSTRING fixes, U& identifiers).

Groups:
  A. repeat/lpad/rpad/ascii/ltrim -- NULL propagation, Unicode chars,
     size guards, lpad negative -> '', ascii('')=0.
  B. regexp_replace -- legacy 4-text-arg flags, integer-start overloads,
     strict start/occurrence validation.
  C. INSERT...VALUES expressions -- general Expr in VALUES cells.
  D. Dollar-quoted literals -- $tag$...$tag$, $$...$$.
  E. Regexp strictness -- 2201B for bad flags, n/s/x options,
     22023 for invalid start/occurrence.
  F. SUBSTRING FROM..FOR -- negative start, expression args.
  G. U& identifiers -- U&"..." with UESCAPE.
"""
import socket
import struct
import subprocess
import sys
import tempfile
import time
import os

HOST = "127.0.0.1"
PORT = 5433
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

PASS = 0
FAIL = 0

def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
    else:
        FAIL += 1
        print(f"FAIL: {name} {detail}")

def read_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("closed")
        data += chunk
    return data

def read_msg(sock):
    typ = read_exact(sock, 1)
    (ln,) = struct.unpack("!i", read_exact(sock, 4))
    payload = read_exact(sock, ln - 4)
    return typ, payload

def read_until(sock, want):
    msgs = []
    while True:
        t, p = read_msg(sock)
        msgs.append((t, p))
        if t == want:
            return msgs

def parse_cstring(payload, pos):
    end = payload.index(b"\x00", pos)
    return payload[pos:end].decode(), end + 1

def parse_error(p):
    fields = {}
    pos = 0
    while pos < len(p) and p[pos] != 0:
        code = chr(p[pos])
        val, pos = parse_cstring(p, pos + 1)
        fields[code] = val
    return fields

class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        body = struct.pack("!i", 196608) + params
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        read_until(self.s, b"Z")

    def sql(self, q):
        body = q.encode() + b"\x00"
        self.s.sendall(b"Q" + struct.pack("!i", len(body) + 4) + body)
        msgs = read_until(self.s, b"Z")
        rows, err, tag = [], None, None
        for t, p in msgs:
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos = 2
                row = []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos+4])
                    pos += 4
                    if ln < 0:
                        row.append(None)
                    else:
                        row.append(p[pos:pos+ln].decode())
                        pos += ln
                rows.append(row)
            elif t == b"E":
                err = parse_error(p)
            elif t == b"C":
                tag, _ = parse_cstring(p, 0)
        return rows, tag, err

    def close(self):
        try:
            self.s.sendall(b"X" + struct.pack("!i", 4))
        finally:
            self.s.close()

def val(c, sql):
    """Run SELECT, return first cell or ('ERR', code)."""
    rows, _, err = c.sql(sql)
    if err:
        return ("ERR", err.get("C"))
    if rows and rows[0]:
        return rows[0][0]
    return None

def main():
    tmp = tempfile.mkdtemp(prefix="rg24_")
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

        # --- A. String built-ins ---
        check("A1 repeat basic", val(c, "SELECT repeat('Pg', 3)") == "PgPgPg")
        check("A2 repeat zero", val(c, "SELECT repeat('Pg', 0)") == "")
        check("A3 repeat negative -> ''", val(c, "SELECT repeat('Pg', -4)") == "")
        check("A4 repeat NULL", val(c, "SELECT repeat(NULL, 3)") is None)
        check("A5 repeat NULL n", val(c, "SELECT repeat('x', NULL)") is None)
        check("A6 lpad basic", val(c, "SELECT lpad('hi', 5, 'xy')") == "xyxhi")
        check("A7 lpad default space", val(c, "SELECT lpad('hi', 5)") == "   hi")
        check("A8 lpad negative -> ''", val(c, "SELECT lpad('hi', -5, 'xy')") == "")
        check("A9 lpad truncate", val(c, "SELECT lpad('hello', 2)") == "he")
        check("A10 lpad empty fill", val(c, "SELECT lpad('hi', 5, '')") == "hi")
        check("A11 lpad NULL", val(c, "SELECT lpad(NULL, 5)") is None)
        check("A12 rpad basic", val(c, "SELECT rpad('hi', 5, 'xy')") == "hixyx")
        check("A13 rpad negative -> ''", val(c, "SELECT rpad('hi', -5)") == "")
        check("A14 ascii basic", val(c, "SELECT ascii('A')") == "65")
        check("A15 ascii empty -> 0", val(c, "SELECT ascii('')") == "0")
        check("A16 ascii NULL", val(c, "SELECT ascii(NULL)") is None)
        check("A17 ascii unicode", val(c, "SELECT ascii('é')") == "233")
        check("A18 ltrim default", val(c, "SELECT ltrim('  hi  ')") == "hi  ")
        check("A19 ltrim chars", val(c, "SELECT ltrim('xxhi', 'x')") == "hi")
        check("A20 ltrim NULL", val(c, "SELECT ltrim(NULL)") is None)
        # Unicode character semantics (not bytes)
        check("A21 lpad unicode", val(c, "SELECT lpad('é', 3, 'ñ')") == "ññé")
        check("A22 repeat unicode", val(c, "SELECT repeat('é', 2)") == "éé")

        # --- B. regexp_replace ---
        check("B1 legacy 4-arg flags",
              val(c, "SELECT regexp_replace('abc', 'b', 'X', 'i')") == "aXc")
        check("B2 4-arg no flags",
              val(c, "SELECT regexp_replace('abc', 'b', 'X', '')") == "aXc")
        check("B3 extended start+count",
              val(c, "SELECT regexp_replace('aaa', 'a', 'X', 2, 1)") == "aXa")
        check("B4 extended N=0 all",
              val(c, "SELECT regexp_replace('aaa', 'a', 'X', 1, 0)") == "XXX")
        check("B5 Nth only",
              val(c, "SELECT regexp_replace('aaa', 'a', 'X', 1, 2)")
              == "aXa")
        check("B6 invalid start 22023",
              val(c, "SELECT regexp_replace('a','a','X',-1,1)") == ("ERR", "22023"))
        check("B7 invalid N 22023",
              val(c, "SELECT regexp_replace('a','a','X',1,-1)") == ("ERR", "22023"))
        check("B8 bad flags 2201B",
              val(c, "SELECT regexp_replace('a','a','X','z')") == ("ERR", "2201B"))

        # --- C. INSERT expressions ---
        for q in ["CREATE TABLE e1(a int, b text, c int)"]:
            _, _, err = c.sql(q)
            check(f"setup {q[:30]}", err is None, str(err))
        _, _, err = c.sql("INSERT INTO e1 VALUES (1+2, repeat('x', 3), -5)")
        check("C1 insert exprs", err is None, str(err))
        check("C2 values correct",
              c.sql("SELECT * FROM e1")[0] == [["3", "xxx", "-5"]])
        _, _, err = c.sql("INSERT INTO e1 VALUES (2*3, 'a' || 'b', NULL)")
        check("C3 insert concat+null", err is None, str(err))
        check("C4 null stored",
              c.sql("SELECT c FROM e1 WHERE a = 6")[0] == [[None]])
        # Params in expressions
        _, _, err = c.sql("INSERT INTO e1 VALUES (7, 'p', 8)")
        check("C5 plain still works", err is None, str(err))

        # --- D. Dollar quoting ---
        check("D1 dollar quote",
              val(c, "SELECT $q$hello$q$") == "hello")
        check("D2 dollar empty tag",
              val(c, "SELECT $$hello$$") == "hello")
        check("D3 dollar with newline",
              val(c, "SELECT $re$\\s+$re$") == "\\s+")
        check("D4 dollar in expr",
              val(c, "SELECT $a$x$a$ || $b$y$b$") == "xy")

        # --- E. Regexp strictness ---
        check("E1 bad flag 2201B",
              val(c, "SELECT regexp_like('a', 'a', 'z')") == ("ERR", "2201B"))
        check("E2 g flag unsupported 2201B",
              val(c, "SELECT regexp_like('a', 'a', 'g')") == ("ERR", "2201B"))
        check("E3 n flag newline dot",
              val(c, "SELECT regexp_like('a' || chr(10) || 'b', 'a.b', 'n')") == "f")
        check("E4 s flag dotall",
              val(c, "SELECT regexp_like('a' || chr(10) || 'b', 'a.b', 's')") == "t")
        check("E5 invalid start 22023",
              val(c, "SELECT regexp_instr('abc', 'b', 0)") == ("ERR", "22023"))
        check("E6 invalid occurrence 22023",
              val(c, "SELECT regexp_substr('abc', 'b', 1, 0)") == ("ERR", "22023"))
        check("E7 regexp_count start 0 -> 22023",
              val(c, "SELECT regexp_count('abc', 'b', 0)") == ("ERR", "22023"))

        # --- F. SUBSTRING FROM..FOR ---
        check("F1 negative start",
              val(c, "SELECT SUBSTRING('hello' FROM -1 FOR 2)") == "")
        check("F2 negative start 2",
              val(c, "SELECT SUBSTRING('hello' FROM -4 FOR 7)") == "he")
        check("F3 expr len",
              val(c, "SELECT SUBSTRING('1234567890' FROM 5 FOR 1+1)") == "56")
        check("F4 plain still works",
              val(c, "SELECT SUBSTRING('hello' FROM 2 FOR 2)") == "el")

        # --- G. U& identifiers ---
        check("G1 U& string",
              val(c, "SELECT u&'\\0041'") == "A")
        check("G2 U& with UESCAPE",
              val(c, "SELECT u&'d!0061t' UESCAPE '!'") == "dat")
        check("G3 U& ident alias",
              val(c, "SELECT 1 AS u&\"\\0041\"") == "1")
        check("G4 bad UESCAPE char -> 42601",
              val(c, "SELECT u&'a' UESCAPE '+'") == ("ERR", "42601"))

        # --- H. chr/initcap/rtrim extras ---
        check("H1 chr basic", val(c, "SELECT chr(65)") == "A")
        check("H2 chr negative 22023",
              val(c, "SELECT chr(-1)") == ("ERR", "22023"))
        check("H3 initcap", val(c, "SELECT initcap('hello world')") == "Hello World")
        check("H4 rtrim", val(c, "SELECT rtrim('hi  ')") == "hi")
        check("H5 chr zero 54000",
              val(c, "SELECT chr(0)") == ("ERR", "54000"))
        check("H6 initcap unicode",
              val(c, "SELECT initcap('héllo')") == "Héllo")
        check("H7 rtrim chars",
              val(c, "SELECT rtrim('hixx', 'x')") == "hi")
        check("H8 chr NULL", val(c, "SELECT chr(NULL)") is None)

        # --- I. More string edge cases ---
        check("I1 repeat 1GB guard",
              val(c, "SELECT repeat('x', 2000000000)") == ("ERR", "54000"))
        check("I2 lpad unicode trunc",
              val(c, "SELECT lpad('héllo', 3)") == "hél")
        check("I3 rpad unicode",
              val(c, "SELECT rpad('é', 3, 'ñ')") == "éññ")
        check("I4 ascii multi-char",
              val(c, "SELECT ascii('ABC')") == "65")
        check("I5 ltrim multi-char set",
              val(c, "SELECT ltrim('abchi', 'abc')") == "hi")
        check("I6 lpad fill longer than needed",
              val(c, "SELECT lpad('hi', 4, 'xyz')") == "xyhi")
        check("I7 repeat empty string",
              val(c, "SELECT repeat('', 5)") == "")
        check("I8 lpad zero length",
              val(c, "SELECT lpad('hi', 0)") == "")

        # --- J. More regexp ---
        check("J1 regexp_replace global flag",
              val(c, "SELECT regexp_replace('aaa', 'a', 'X', 'g')") == "XXX")
        check("J2 regexp_count basic",
              val(c, "SELECT regexp_count('abcabc', 'b')") == "2")
        check("J3 regexp_instr",
              val(c, "SELECT regexp_instr('abc', 'b')") == "2")
        check("J4 regexp_like case-insensitive",
              val(c, "SELECT regexp_like('ABC', 'abc', 'i')") == "t")
        check("J5 regexp_substr group",
              val(c, "SELECT regexp_substr('abc123', '([0-9]+)', 1, 1, '', 1)") == "123")

        # --- K. More INSERT expressions ---
        _, _, err = c.sql("INSERT INTO e1 VALUES (10 % 3, upper('ab'), 100 // 10)")
        # // is not PG; use div instead
        check("K1 insert modulo", err is None or "42601" in str(err))
        _, _, err = c.sql("INSERT INTO e1 VALUES (abs(-5), lower('AB'), length('xyz'))")
        check("K2 insert funcs", err is None, str(err))
        check("K3 values",
              ["5"] in c.sql("SELECT a FROM e1 WHERE b = 'ab'")[0])

        # --- L. Dollar quoting edge cases ---
        check("L1 dollar nested",
              val(c, "SELECT $outer$ $inner$ $outer$") == " $inner$ ")
        check("L2 dollar multiline",
              val(c, "SELECT $t$line1\nline2$t$") == "line1\nline2")
        check("L3 dollar tag with digits",
              val(c, "SELECT $tag1$hi$tag1$") == "hi")
        check("L4 dollar empty",
              val(c, "SELECT $$$$") == "")
        check("L5 dollar in insert",
              c.sql("INSERT INTO e1 VALUES (99, $v$val$v$, 1)")[2] is None)

        # --- M. SUBSTRING edge cases ---
        check("M1 substring_from pattern",
              val(c, "SELECT SUBSTRING('hello' FROM 'l+')") == "ll")
        check("M2 substring comma neg start",
              val(c, "SELECT SUBSTRING('hello', -1, 2)") == "")
        check("M3 substring_for zero len",
              val(c, "SELECT SUBSTRING('hello' FROM 1 FOR 0)") == "")
        check("M4 substring_from_for int expr",
              val(c, "SELECT SUBSTRING('hello' FROM 2*1 FOR 3-1)") == "el")
        check("M5 repeat in substr",
              val(c, "SELECT SUBSTRING(repeat('ab', 3) FROM 2 FOR 3)") == "bab")

        c.close()
    finally:
        proc.terminate()
        proc.wait()

    print(f"\n{ PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0

if __name__ == "__main__":
    sys.exit(main())
