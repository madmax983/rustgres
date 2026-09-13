#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.23 (JOIN ... USING / NATURAL merged
columns, and table column alias lists).

Groups:
  A. USING inner merge -- single visible merged key first, then left
     rest, then right rest; unqualified key resolves (no 42702);
     qualified originals stay reachable; qual.* in side order.
  B. USING outer joins -- LEFT keeps left key, RIGHT takes right key,
     FULL coalesces; null-extended non-key columns.
  C. Multiple USING keys -- merged in USING order; partial-key mismatch
     is not a match; unknown key column -> 42703.
  D. NATURAL JOIN -- merges shared columns; chains of 3 tables.
  E. JOIN ... USING (cols) AS x -- x.* exposes only merged columns;
     x.nonkey -> 42703; unqualified refs still work.
  F. Parenthesized whole-join aliases -- (a JOIN b ...) AS x requalifies
     to x, hides inner names; column alias lists rename positionally.
  G. Table column alias lists -- base tables, derived subqueries, CTEs,
     VALUES; fewer aliases than columns is fine.
  H. Ambiguity is still reported where PostgreSQL reports it.
  I. Merged keys in WHERE / GROUP BY / ORDER BY.
  J. USING error cases -- 42703 names the missing column.
  K. Regression -- ON joins and comma joins keep old column order.

Boots its own server on 127.0.0.1:5433 with a scratch data dir.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test23.py`.
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


def allrows(c, q):
    rows, _, err = c.sql(q)
    if err:
        return f"ERR:{err}"
    return rows


def colnames(c, q):
    _, cols, err = c.sql(q)
    if err:
        return f"ERR:{err}"
    return cols


def errcode(c, q):
    _, _, err = c.sql(q)
    return err or ""


def main():
    tmp = tempfile.mkdtemp(prefix="rg23_")
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

        for q in [
            "CREATE TABLE j1(i int, j int, t text)",
            "CREATE TABLE j2(i int, k int)",
            "INSERT INTO j1 VALUES (1,10,'a'),(2,20,'b'),(4,40,'d')",
            "INSERT INTO j2 VALUES (1,100),(3,300),(4,400)",
            "CREATE TABLE m1(a int, b int, x text)",
            "CREATE TABLE m2(a int, b int, y text)",
            "INSERT INTO m1 VALUES (1,2,'p'),(3,4,'q')",
            "INSERT INTO m2 VALUES (1,2,'u'),(3,9,'v')",
        ]:
            _, _, err = c.sql(q)
            check(f"setup: {q[:40]}", err is None, f"err={err}")

        # --- A. USING inner merge ---
        check("A1 merged key first",
              colnames(c, "SELECT * FROM j1 JOIN j2 USING (i)") == ["i", "j", "t", "k"])
        check("A2 inner rows",
              allrows(c, "SELECT * FROM j1 JOIN j2 USING (i) ORDER BY i")
              == [["1", "10", "a", "100"], ["4", "40", "d", "400"]])
        check("A3 unqualified key, no 42702",
              allrows(c, "SELECT i FROM j1 JOIN j2 USING (i) ORDER BY i")
              == [["1"], ["4"]])
        check("A4 qualified originals",
              allrows(c, "SELECT j1.i, j2.i FROM j1 JOIN j2 USING (i) ORDER BY j1.i")
              == [["1", "1"], ["4", "4"]])
        check("A5 left qual.* in side order",
              colnames(c, "SELECT j1.* FROM j1 JOIN j2 USING (i)") == ["i", "j", "t"])
        check("A6 right qual.* in side order",
              colnames(c, "SELECT j2.* FROM j1 JOIN j2 USING (i)") == ["i", "k"])
        check("A7 left qual.* rows",
              allrows(c, "SELECT j1.* FROM j1 JOIN j2 USING (i) ORDER BY j1.i")
              == [["1", "10", "a"], ["4", "40", "d"]])
        check("A8 star shows key once",
              colnames(c, "SELECT * FROM j1 JOIN j2 USING (i)").count("i") == 1)
        check("A9 explicit cols",
              colnames(c, "SELECT t, k, i, j FROM j1 JOIN j2 USING (i)") == ["t", "k", "i", "j"])
        check("A10 no 42702 in join cond recheck",
              errcode(c, "SELECT i FROM j1 JOIN j2 USING (i)") == "")

        # --- B. USING outer joins ---
        check("B1 left rows",
              allrows(c, "SELECT * FROM j1 LEFT JOIN j2 USING (i) ORDER BY i")
              == [["1", "10", "a", "100"], ["2", "20", "b", None], ["4", "40", "d", "400"]])
        check("B2 left merged key from left",
              one(c, "SELECT i FROM j1 LEFT JOIN j2 USING (i) WHERE j = 20") == "2")
        check("B3 right rows",
              allrows(c, "SELECT * FROM j1 RIGHT JOIN j2 USING (i) ORDER BY i")
              == [["1", "10", "a", "100"], ["3", None, None, "300"], ["4", "40", "d", "400"]])
        check("B4 right merged key from right",
              one(c, "SELECT i FROM j1 RIGHT JOIN j2 USING (i) WHERE k = 300") == "3")
        check("B5 full rows",
              allrows(c, "SELECT * FROM j1 FULL JOIN j2 USING (i) ORDER BY i")
              == [["1", "10", "a", "100"], ["2", "20", "b", None],
                  ["3", None, None, "300"], ["4", "40", "d", "400"]])
        check("B6 full coalesces left key",
              one(c, "SELECT i FROM j1 FULL JOIN j2 USING (i) WHERE t = 'b'") == "2")
        check("B7 full coalesces right key",
              one(c, "SELECT i FROM j1 FULL JOIN j2 USING (i) WHERE k = 300") == "3")
        check("B8 full outer row count", len(allrows(c, "SELECT * FROM j1 FULL JOIN j2 USING (i)")) == 4)
        check("B9 left row count", len(allrows(c, "SELECT * FROM j1 LEFT JOIN j2 USING (i)")) == 3)
        check("B10 right row count", len(allrows(c, "SELECT * FROM j1 RIGHT JOIN j2 USING (i)")) == 3)

        # --- C. Multiple USING keys ---
        check("C1 multi-key order",
              colnames(c, "SELECT * FROM m1 JOIN m2 USING (a, b)") == ["a", "b", "x", "y"])
        check("C2 multi-key rows",
              allrows(c, "SELECT * FROM m1 JOIN m2 USING (a, b)") == [["1", "2", "p", "u"]])
        check("C3 partial key mismatch is not a match",
              allrows(c, "SELECT * FROM m1 FULL JOIN m2 USING (a, b) ORDER BY a, b")
              == [["1", "2", "p", "u"], ["3", "4", "q", None], ["3", "9", None, "v"]])
        check("C4 unqualified multi keys",
              allrows(c, "SELECT a, b FROM m1 JOIN m2 USING (a, b)") == [["1", "2"]])
        check("C5 qualified multi originals",
              allrows(c, "SELECT m1.a, m2.a, m1.b, m2.b FROM m1 JOIN m2 USING (a, b)")
              == [["1", "1", "2", "2"]])
        check("C6 unknown key -> 42703",
              errcode(c, "SELECT * FROM m1 JOIN m2 USING (a, c)") == "42703")
        check("C7 key missing on left -> 42703",
              errcode(c, "SELECT * FROM m1 JOIN j1 USING (a)") == "42703")
        check("C8 natural multi-key",
              colnames(c, "SELECT * FROM m1 NATURAL JOIN m2") == ["a", "b", "x", "y"])

        # --- D. NATURAL JOIN ---
        check("D1 natural cols",
              colnames(c, "SELECT * FROM j1 NATURAL JOIN j2") == ["i", "j", "t", "k"])
        check("D2 natural rows",
              allrows(c, "SELECT * FROM j1 NATURAL JOIN j2 ORDER BY i")
              == [["1", "10", "a", "100"], ["4", "40", "d", "400"]])
        check("D3 natural unqualified",
              one(c, "SELECT i FROM j1 NATURAL JOIN j2 WHERE j = 10") == "1")
        check("D4 setup j3",
              errcode(c, "CREATE TABLE j3(i int, m int)") == "")
        _, _, err = c.sql("INSERT INTO j3 VALUES (1,1000),(4,4000)")
        check("D4 setup j3 rows", err is None, f"err={err}")
        check("D5 three-table chain cols",
              colnames(c, "SELECT * FROM j1 NATURAL JOIN j2 NATURAL JOIN j3")
              == ["i", "j", "t", "k", "m"])
        check("D6 three-table chain rows",
              allrows(c, "SELECT * FROM j1 NATURAL JOIN j2 NATURAL JOIN j3 ORDER BY i")
              == [["1", "10", "a", "100", "1000"], ["4", "40", "d", "400", "4000"]])
        check("D7 natural left join",
              allrows(c, "SELECT * FROM j1 NATURAL LEFT JOIN j2 ORDER BY i")
              == [["1", "10", "a", "100"], ["2", "20", "b", None], ["4", "40", "d", "400"]])

        # --- E. JOIN ... USING (cols) AS x ---
        check("E1 x.i works",
              allrows(c, "SELECT x.i FROM j1 JOIN j2 USING (i) AS x ORDER BY x.i")
              == [["1"], ["4"]])
        check("E2 x.* is only merged cols",
              colnames(c, "SELECT x.* FROM j1 JOIN j2 USING (i) AS x") == ["i"])
        check("E3 x.* rows",
              allrows(c, "SELECT x.* FROM j1 JOIN j2 USING (i) AS x ORDER BY x.i")
              == [["1"], ["4"]])
        check("E4 x.nonkey -> 42703",
              errcode(c, "SELECT x.t FROM j1 JOIN j2 USING (i) AS x") == "42703")
        check("E5 x.j -> 42703",
              errcode(c, "SELECT x.j FROM j1 JOIN j2 USING (i) AS x") == "42703")
        check("E6 unqualified through using-alias join",
              allrows(c, "SELECT i FROM j1 JOIN j2 USING (i) AS x ORDER BY i")
              == [["1"], ["4"]])
        check("E7 multi-key x.*",
              colnames(c, "SELECT x.* FROM m1 JOIN m2 USING (a, b) AS x") == ["a", "b"])
        check("E8 multi-key x.* rows",
              allrows(c, "SELECT x.* FROM m1 JOIN m2 USING (a, b) AS x") == [["1", "2"]])
        check("E9 bare * unaffected by using alias",
              colnames(c, "SELECT * FROM j1 JOIN j2 USING (i) AS x") == ["i", "j", "t", "k"])
        check("E10 source quals still work",
              allrows(c, "SELECT j1.t FROM j1 JOIN j2 USING (i) AS x ORDER BY j1.i")
              == [["a"], ["d"]])

        # --- F. Parenthesized whole-join aliases ---
        check("F1 paren join alias cols",
              colnames(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x") == ["i", "j", "t", "k"])
        check("F2 paren join alias rows",
              allrows(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x ORDER BY i")
              == [["1", "10", "a", "100"], ["4", "40", "d", "400"]])
        check("F3 x.* over whole join",
              colnames(c, "SELECT x.* FROM (j1 JOIN j2 USING (i)) AS x") == ["i", "j", "t", "k"])
        check("F4 inner names hidden",
              errcode(c, "SELECT j1.i FROM (j1 JOIN j2 USING (i)) AS x") != "")
        check("F5 whole-join col alias list",
              colnames(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x(p, q, r, s)")
              == ["p", "q", "r", "s"])
        check("F6 whole-join alias list rows",
              allrows(c, "SELECT p, s FROM (j1 JOIN j2 USING (i)) AS x(p, q, r, s) ORDER BY p")
              == [["1", "100"], ["4", "400"]])
        check("F7 fewer aliases than columns",
              colnames(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x(p, q)") == ["p", "q", "t", "k"])
        check("F8 aliased join feeds outer join",
              allrows(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x JOIN j3 ON x.i = j3.i ORDER BY x.i")
              == [["1", "10", "a", "100", "1", "1000"], ["4", "40", "d", "400", "4", "4000"]])
        check("F9 using-alias inside parens",
              allrows(c, "SELECT y.i FROM (j1 JOIN j2 USING (i) AS y) AS z ORDER BY y.i") == []
              or errcode(c, "SELECT y.i FROM (j1 JOIN j2 USING (i) AS y) AS z") != "")
        check("F10 nested paren joins",
              allrows(c, "SELECT w.j, w.m FROM ((j1 JOIN j2 USING (i)) AS x JOIN j3 ON x.i = j3.i) AS w ORDER BY w.j")
              == [["10", "1000"], ["40", "4000"]])

        # --- G. Table column alias lists ---
        check("G1 base rename cols",
              colnames(c, "SELECT * FROM j1 AS t(a, b, c)") == ["a", "b", "c"])
        check("G2 base rename unqualified",
              allrows(c, "SELECT a FROM j1 AS t(a, b, c) ORDER BY a") == [["1"], ["2"], ["4"]])
        check("G3 base rename qualified",
              allrows(c, "SELECT t.a, t.b FROM j1 AS t(a, b, c) ORDER BY t.a")
              == [["1", "10"], ["2", "20"], ["4", "40"]])
        check("G4 fewer aliases keeps rest",
              colnames(c, "SELECT * FROM j1 AS t(a)") == ["a", "j", "t"])
        check("G5 old names gone",
              errcode(c, "SELECT i FROM j1 AS t(a, b, c)") == "42703")
        check("G6 derived rename",
              colnames(c, "SELECT * FROM (SELECT 1 AS a, 2 AS b) AS s(x, y)") == ["x", "y"])
        check("G7 derived rename rows",
              allrows(c, "SELECT x FROM (SELECT 1 AS a, 2 AS b) AS s(x, y)") == [["1"]])
        check("G8 CTE rename",
              allrows(c, "WITH w(a, b) AS (SELECT 1, 2) SELECT * FROM w") == [["1", "2"]])
        check("G9 VALUES rename",
              allrows(c, "SELECT * FROM (VALUES (1,2),(3,4)) AS v(x, y) ORDER BY x")
              == [["1", "2"], ["3", "4"]])
        check("G10 aliases feed USING",
              allrows(c, "SELECT * FROM j1 AS t(x, y, z) JOIN j2 AS u(x, w) USING (x) ORDER BY x")
              == [["1", "10", "a", "100"], ["4", "40", "d", "400"]])

        # --- H. Ambiguity still reported ---
        check("H1 cross join dup -> 42702",
              errcode(c, "SELECT i FROM j1, j2") == "42702")
        check("H2 on-join dup -> 42702",
              errcode(c, "SELECT i FROM j1 JOIN j2 ON j1.i = j2.i") == "42702")
        check("H3 qualified disambiguates",
              errcode(c, "SELECT j1.i FROM j1, j2") == "")
        check("H4 using resolves, on does not",
              errcode(c, "SELECT i FROM j1 JOIN j2 USING (i)") == ""
              and errcode(c, "SELECT i FROM j1 JOIN j2 ON j1.i = j2.i") == "42702")

        # --- I. Merged keys in clauses ---
        check("I1 where on merged",
              allrows(c, "SELECT i FROM j1 JOIN j2 USING (i) WHERE i > 1 ORDER BY i") == [["4"]])
        check("I2 group by merged",
              allrows(c, "SELECT i, count(*) FROM j1 JOIN j2 USING (i) GROUP BY i ORDER BY i")
              == [["1", "1"], ["4", "1"]])
        check("I3 order by merged",
              allrows(c, "SELECT t FROM j1 JOIN j2 USING (i) ORDER BY i DESC") == [["d"], ["a"]])
        check("I4 having on merged",
              allrows(c, "SELECT i, count(*) FROM j1 JOIN j2 USING (i) GROUP BY i HAVING i > 1")
              == [["4", "1"]])
        check("I5 merged in join cond of outer join",
              allrows(c, "SELECT x.i FROM (j1 JOIN j2 USING (i)) AS x JOIN j3 ON x.i = j3.i ORDER BY x.i")
              == [["1"], ["4"]])

        # --- J. USING error cases ---
        check("J1 unknown using col -> 42703",
              errcode(c, "SELECT * FROM j1 JOIN j2 USING (zzz)") == "42703")
        check("J2 using col missing left -> 42703",
              errcode(c, "SELECT * FROM j1 JOIN j2 USING (k)") == "42703")
        check("J3 using col missing right -> 42703",
              errcode(c, "SELECT * FROM j1 JOIN j2 USING (j)") == "42703")
        check("J4 setup e1", errcode(c, "CREATE TABLE e1(a int)") == "")
        _, _, err = c.sql("CREATE TABLE e2(b int)")
        check("J4 setup e2", err is None)
        _, _, err = c.sql("INSERT INTO e1 VALUES (1),(2)")
        check("J4 setup rows", err is None)
        _, _, err = c.sql("INSERT INTO e2 VALUES (10),(20)")
        check("J4 setup rows2", err is None)
        check("J5 natural no common -> cross join",
              len(allrows(c, "SELECT * FROM e1 NATURAL JOIN e2")) == 4)

        # --- K. Regression: ON / comma joins unchanged ---
        check("K1 on-join col order",
              colnames(c, "SELECT * FROM j1 JOIN j2 ON j1.i = j2.i ORDER BY j1.i")
              == ["i", "j", "t", "i", "k"])
        check("K2 comma col order",
              colnames(c, "SELECT * FROM j1, j2") == ["i", "j", "t", "i", "k"])
        check("K3 on-join still errors ambiguous unqualified",
              errcode(c, "SELECT i FROM j1 JOIN j2 ON j1.i = j2.i") == "42702")
        check("K4 plain table star",
              colnames(c, "SELECT * FROM j1") == ["i", "j", "t"])
        check("K5 mixed using then on",
              allrows(c, "SELECT * FROM j1 JOIN j2 USING (i) JOIN j3 ON j3.i = j1.i ORDER BY j1.i")
              == [["1", "10", "a", "100", "1", "1000"], ["4", "40", "d", "400", "4", "4000"]])

        # --- L. column alias list arity (PostgreSQL 42601) ---
        check("L1 whole-join too many -> 42601",
        errcode(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x(a,b,c,d,e,f)") == "42601")
        check("L2 base table too many -> 42601",
        errcode(c, "SELECT * FROM j1 AS t(a,b,c,d)") == "42601")
        check("L3 derived too many -> 42601",
        errcode(c, "SELECT * FROM (SELECT 1 AS x) AS s(a,b)") == "42601")
        check("L4 values too many -> 42601",
        errcode(c, "SELECT * FROM (VALUES (1,2)) AS v(a,b,c)") == "42601")
        check("L5 cte too many -> 42601",
        errcode(c, "WITH c(a,b,c) AS (SELECT 1, 2) SELECT * FROM c") == "42601")
        check("L6 exact count still works",
        colnames(c, "SELECT * FROM j1 AS t(a,b,c)") == ["a", "b", "c"])
        check("L7 fewer than columns still works",
        colnames(c, "SELECT * FROM j1 AS t(a)") == ["a", "j", "t"])
        check("L8 col list without alias -> 42601",
        errcode(c, "SELECT * FROM (j1) (a)") == "42601")
        check("L9 whole-join exact count works",
        colnames(c, "SELECT * FROM (j1 JOIN j2 USING (i)) AS x(a,b,c,d)") == ["a", "b", "c", "d"])


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
