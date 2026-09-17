#!/usr/bin/env python3
"""protocol_test44.py — v0.44 UNION / INTERSECT / EXCEPT (with ALL).

v0.44 implements PostgreSQL 19 set operations: UNION [ALL|DISTINCT],
INTERSECT [ALL|DISTINCT], and EXCEPT [ALL|DISTINCT], with PG's
precedence (INTERSECT binds tighter than UNION/EXCEPT), left
associativity, parenthesized branches, root ORDER BY / LIMIT / OFFSET,
duplicate semantics (NULLs not distinct), numeric type widening, and
character-family resolution to text.

This test manages its own server on port 5546 (so it never collides
with the conformance runner) and is RED on the v0.43 base, GREEN on the
v0.44 branch.

Sections:
  A. UNION: duplicate elimination, ALL preservation, DISTINCT.
  B. INTERSECT / INTERSECT ALL multiset semantics.
  C. EXCEPT / EXCEPT ALL multiset semantics.
  D. NULL duplicate semantics (NULLs not distinct).
  E. Type resolution: numeric widening, char->text, name from left.
  F. Errors: column-count mismatch (42804), type mismatch (42804).
  G. Precedence and associativity: INTERSECT tighter, left-assoc,
     parenthesized override.
  H. Root ORDER BY (ordinal and name), LIMIT, OFFSET.
  I. Set-ops in derived tables, CTEs, INSERT..SELECT.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5546
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
    data_dir = tempfile.mkdtemp(prefix="rg44_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        def q(sql):
            cols, rows, err, msg = c.do_sql(sql)
            return cols, rows, err, msg

        def vals(sql):
            cols, rows, err, msg = q(sql)
            assert not err, f"{sql} -> {err}: {msg}"
            return [r[0] for r in rows]

        # -- A. UNION ---------------------------------------------------
        check("A1 union dedups", vals("SELECT 1 UNION SELECT 1") == ["1"])
        check("A2 union keeps distinct",
              vals("SELECT 1 UNION SELECT 2") == ["1", "2"])
        check("A3 union all keeps dups",
              vals("SELECT 1 UNION ALL SELECT 1") == ["1", "1"])
        check("A4 union distinct dedups",
              vals("SELECT 1 UNION DISTINCT SELECT 1") == ["1"])
        check("A5 union chain",
              vals("SELECT 1 UNION SELECT 2 UNION SELECT 2") == ["1", "2"])

        # -- B. INTERSECT -----------------------------------------------
        check("B1 intersect", vals("SELECT 1 INTERSECT SELECT 1") == ["1"])
        check("B2 intersect empty",
              vals("SELECT 1 INTERSECT SELECT 2") == [])
        # INTERSECT binds tighter: 1 UNION ALL (1 INTERSECT ALL 1) = {1,1}.
        check("B3 intersect all min multiplicity",
              vals("SELECT 1 UNION ALL SELECT 1 INTERSECT ALL SELECT 1") == ["1", "1"])
        # (1 INTERSECT ALL 1) UNION ALL 1 = {1,1}.
        check("B4 intersect all union",
              vals("SELECT 1 INTERSECT ALL SELECT 1 UNION ALL SELECT 1") == ["1", "1"])

        # -- C. EXCEPT --------------------------------------------------
        check("C1 except", vals("SELECT 1 EXCEPT SELECT 2") == ["1"])
        check("C2 except removes",
              vals("SELECT 1 EXCEPT SELECT 1") == [])
        check("C3 except all subtracts",
              vals("SELECT 1 UNION ALL SELECT 1 EXCEPT ALL SELECT 1") == ["1"])
        # (1 EXCEPT ALL 1) UNION ALL 1 = {1}.
        check("C4 except all then union",
              vals("SELECT 1 EXCEPT ALL SELECT 1 UNION ALL SELECT 1") == ["1"])

        # -- D. NULLs ---------------------------------------------------
        check("D1 null union dedups",
              vals("SELECT NULL UNION SELECT NULL") == [None])
        check("D2 null intersect",
              vals("SELECT NULL INTERSECT SELECT NULL") == [None])
        check("D3 null except removes",
              vals("SELECT NULL EXCEPT SELECT NULL") == [])

        # -- E. Types ---------------------------------------------------
        cols, rows, err, _ = q("SELECT 1 AS two UNION SELECT 2")
        check("E1 names from left", cols == ["two"] and not err)
        cols, rows, err, _ = q("SELECT 1 UNION SELECT 1.5")
        check("E2 int+numeric widens",
              not err and rows == [["1"], ["1.5"]])
        # Character family resolves to text.
        cols, rows, err, _ = q(
            "SELECT 'a'::varchar(3) UNION SELECT 'b'::text")
        check("E3 varchar+text -> text", not err and len(rows) == 2)

        # -- F. Errors --------------------------------------------------
        _, _, err, _ = q("SELECT 1 UNION SELECT 1, 2")
        check("F1 column count mismatch", err == "42804")
        _, _, err, _ = q("SELECT 1 INTERSECT SELECT 'x'")
        check("F2 type mismatch", err == "42804")
        _, _, err, _ = q("SELECT 1 EXCEPT SELECT true")
        check("F3 type mismatch except", err == "42804")

        # -- G. Precedence ----------------------------------------------
        # INTERSECT binds tighter: 1 UNION (1 INTERSECT 2) = {1}.
        check("G1 intersect tighter",
              vals("SELECT 1 UNION SELECT 1 INTERSECT SELECT 2") == ["1"])
        # (1 INTERSECT 1) UNION 2 = {1, 2}.
        check("G2 left assoc",
              vals("SELECT 1 INTERSECT SELECT 1 UNION SELECT 2") == ["1", "2"])
        # Parentheses override: (1 UNION 1) INTERSECT 2 = {}.
        check("G3 parens override",
              vals("(SELECT 1 UNION SELECT 1) INTERSECT SELECT 2") == [])
        # Top-level parenthesized query with trailing set-op.
        check("G4 top paren chain",
              vals("(SELECT 1 UNION SELECT 2) UNION SELECT 3") == ["1", "2", "3"])

        # -- H. ORDER BY / LIMIT / OFFSET --------------------------------
        check("H1 order by ordinal",
              vals("SELECT 2 AS a UNION SELECT 1 AS a ORDER BY 1") == ["1", "2"])
        check("H2 order by name",
              vals("SELECT 2 AS a UNION SELECT 1 AS a ORDER BY a") == ["1", "2"])
        check("H3 order by desc",
              vals("SELECT 1 UNION SELECT 2 ORDER BY 1 DESC") == ["2", "1"])
        check("H4 limit",
              vals("SELECT 1 UNION SELECT 2 ORDER BY 1 LIMIT 1") == ["1"])
        check("H5 offset",
              vals("SELECT 1 UNION SELECT 2 ORDER BY 1 OFFSET 1") == ["2"])
        check("H6 limit+offset",
              vals("SELECT 1 UNION SELECT 2 UNION SELECT 3 ORDER BY 1 LIMIT 1 OFFSET 1")
              == ["2"])

        # -- I. Nesting --------------------------------------------------
        check("I1 derived table",
              vals("SELECT * FROM (SELECT 1 AS x UNION SELECT 2 AS x) t ORDER BY x")
              == ["1", "2"])
        cols, rows, err, _ = q(
            "WITH u AS (SELECT 1 AS x UNION SELECT 2 AS x) SELECT * FROM u ORDER BY x")
        check("I2 cte", not err and [r[0] for r in rows] == ["1", "2"])
        cols, rows, err, msg = q("CREATE TEMP TABLE t44 (x int)")
        check("I3 create temp table", not err)
        cols, rows, err, _ = q("INSERT INTO t44 SELECT 1 AS x UNION SELECT 1 AS x")
        check("I4 insert..select dedups", not err)
        cols, rows, err, _ = q("SELECT * FROM t44")
        check("I5 inserted one row", not err and rows == [["1"]])
        cols, rows, err, _ = q("INSERT INTO t44 SELECT 2 UNION SELECT 2")
        check("I6 insert..select union", not err)
        cols, rows, err, _ = q("SELECT * FROM t44 ORDER BY x")
        check("I7 rows deduped",
              not err and [r[0] for r in rows] == ["1", "2"])

        c.close()
    finally:
        stop_server(proc)

    print(f"protocol_test44: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
