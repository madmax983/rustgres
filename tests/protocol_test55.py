#!/usr/bin/env python3
"""Protocol 55: ORDER BY ... USING, standalone VALUES, and TABLE.

v0.54 implements three PG19 query-language behaviors, grounded in
REL_19_STABLE gram.y:

* `ORDER BY a_expr USING qual_Op` (gram.y `sortby`): the sort operator
  is exclusive with ASC/DESC/NULLS. `<` sorts ascending, `>` sorts
  descending — the btree sort operators. Anything else (e.g. `+`) is
  rejected with 0A000 (feature not supported), not a syntax error.
* Standalone `VALUES (...) , (...)` as a top-level query (gram.y
  `simple_select: values_clause`): equivalent to
  `SELECT * FROM (VALUES ...) AS "_values"` — column names column1,
  column2, ... and PG's row-list type unification.
* Standalone `TABLE name` as a top-level query (gram.y
  `simple_select: TABLE relation_expr`): equivalent to
  `SELECT * FROM name`.
* Both forms also work as set-operation branches:
  `VALUES ... UNION ALL SELECT ...`, `VALUES ... UNION ALL TABLE t`.

RED on v0.53 (base 6e108f28): every case below raises 42601
(syntax error at or near "using" / "values" / "table").
GREEN on v0.54: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5555
DATA_DIR = "/tmp/rg55proto"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d


def connect():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"Z":
            break
        if t == b"E":
            raise RuntimeError(f"startup failed: {b!r}")
    return s


def run_sql(s, sql):
    s.sendall(b"Q" + struct.pack("!i", len(sql.encode()) + 5) + sql.encode() + b"\x00")
    rows = []
    tag = None
    err = None
    errcode = None
    oids = []
    cols = []
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"T":
            (n,) = struct.unpack("!h", b[:2])
            pos = 2
            for _ in range(n):
                z = b.index(b"\x00", pos)
                cols.append(b[pos:z].decode())
                pos = z + 1
                (oid,) = struct.unpack("!i", b[pos + 6:pos + 10])
                pos += 18
                oids.append(oid)
        elif t == b"D":
            (n,) = struct.unpack("!h", b[:2])
            pos = 2
            row = []
            for _ in range(n):
                (ln2,) = struct.unpack("!i", b[pos:pos + 4])
                pos += 4
                if ln2 == -1:
                    row.append(None)
                else:
                    row.append(b[pos:pos + ln2].decode())
                    pos += ln2
            rows.append(row)
        elif t == b"C":
            tag = b.split(b"\x00")[0].decode()
        elif t == b"E":
            i = 0
            msg = ""
            code = ""
            while i < len(b) and b[i] != 0:
                f = chr(b[i])
                i += 1
                j = b.find(b"\x00", i)
                v = b[i:j].decode(errors="replace")
                i = j + 1
                if f == "M":
                    msg = v
                elif f == "C":
                    code = v
            err = msg
            errcode = code
        elif t == b"Z":
            break
    return rows, tag, err, errcode, oids, cols


SETUP = [
    "CREATE TABLE t55 (a int, b text)",
    "INSERT INTO t55 VALUES (2, 'two'), (1, 'one'), (3, 'three')",
]


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg55proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        for sql in SETUP:
            rows, tag, err, code, oids, cols = run_sql(s, sql)
            if code is not None:
                print(f"SETUP FAILED: {sql}: {code} {err}")
                raise SystemExit(2)
        passed = 0
        failed = 0

        # (sql, want_rows_or_None, want_tag_or_None, want_code_or_None,
        #  want_msg_contains_or_None, want_oids_or_None, want_cols_or_None,
        #  description)
        # OIDs: 23=int4, 25=text, 1700=numeric
        cases = [
            # --- ORDER BY ... USING (PG19 sortby) ---
            ("SELECT x FROM (VALUES (3),(1),(2)) AS v(x) ORDER BY x USING <",
             [["1"], ["2"], ["3"]], None, None, None, None, None,
             "USING <: ascending (v0.53: 42601)"),
            ("SELECT x FROM (VALUES (3),(1),(2)) AS v(x) ORDER BY x USING >",
             [["3"], ["2"], ["1"]], None, None, None, None, None,
             "USING >: descending (v0.53: 42601)"),
            ("SELECT a, b FROM t55 ORDER BY a USING <, b USING >",
             [["1", "one"], ["2", "two"], ["3", "three"]], None, None, None, None, None,
             "multiple USING terms, mixed directions"),
            ("SELECT a FROM t55 ORDER BY a USING >",
             [["3"], ["2"], ["1"]], None, None, None, None, None,
             "USING > on a table column"),
            ("SELECT 1 AS one ORDER BY one USING <",
             [["1"]], None, None, None, None, None,
             "USING < on an output alias"),
            ("SELECT 1 ORDER BY 1 USING +",
             None, None, "0A000", "not supported", None, None,
             "non-sort operator after USING: 0A000 (v0.53: 42601)"),
            # --- standalone VALUES ---
            ("VALUES (1,2), (3,4+4), (7,77.7)",
             [["1", "2"], ["3", "8"], ["7", "77.7"]], "SELECT 3", None, None,
             [23, 1700], ["column1", "column2"],
             "standalone VALUES: PG column names + type unification (v0.53: 42601)"),
            ("VALUES (1),(2) ORDER BY 1 USING >",
             [["2"], ["1"]], None, None, None, None, None,
             "VALUES with ORDER BY ... USING tail"),
            ("VALUES (1,2) UNION ALL SELECT 3,4",
             [["1", "2"], ["3", "4"]], None, None, None, None, None,
             "VALUES as a set-op branch (v0.53: 42601)"),
            # --- standalone TABLE ---
            ("TABLE t55",
             [["2", "two"], ["1", "one"], ["3", "three"]], "SELECT 3", None, None,
             None, ["a", "b"],
             "TABLE t: all rows, real column names (v0.53: 42601)"),
            ("TABLE t55 ORDER BY a USING <",
             [["1", "one"], ["2", "two"], ["3", "three"]], None, None, None,
             None, None,
             "TABLE t with ORDER BY ... USING tail"),
            ("VALUES (9,'nine') UNION ALL TABLE t55",
             [["9", "nine"], ["2", "two"], ["1", "one"], ["3", "three"]],
             None, None, None, None, None,
             "TABLE t as a set-op branch (v0.53: 42601)"),
            # --- IN (VALUES ...) ---
            ("SELECT a FROM t55 WHERE a IN (VALUES (1), (3)) ORDER BY a",
             [["1"], ["3"]], None, None, None, None, None,
             "scalar IN (VALUES ...) desugars to ORs (v0.53: 42883)"),
            ("SELECT a FROM t55 WHERE a NOT IN (VALUES (1), (3)) ORDER BY a",
             [["2"]], None, None, None, None, None,
             "scalar NOT IN (VALUES ...)"),
            # --- ORDER BY ... USING with explicit NULLS ordering (PG19
            # allows NULLS FIRST/LAST after USING; only ASC/DESC clash) ---
            ("CREATE TABLE t55n (a int, b text)",
             None, "CREATE TABLE", None, None, None, None,
             "null-ordering fixture"),
            ("INSERT INTO t55n VALUES (1, 'x'), (NULL, 'y'), (2, 'z')",
             None, "INSERT 0 3", None, None, None, None,
             "null-ordering fixture rows"),
            ("SELECT a FROM t55n ORDER BY a USING < NULLS FIRST",
             [[None], ["1"], ["2"]], None, None, None, None, None,
             "USING < NULLS FIRST: nulls first, then ascending"),
            ("SELECT a FROM t55n ORDER BY a USING > NULLS LAST",
             [["2"], ["1"], [None]], None, None, None, None, None,
             "USING > NULLS LAST: descending, nulls last"),
            ("DROP TABLE t55n",
             None, "DROP TABLE", None, None, None, None,
             "null-ordering fixture cleanup"),
            ("SELECT a, b FROM t55 WHERE (a, b) IN (VALUES (1, 'one'), (3, 'three')) ORDER BY a",
             [["1", "one"], ["3", "three"]], None, None, None, None, None,
             "row-wise IN (VALUES ...) (v0.53: 42601)"),
            ("SELECT a FROM t55 WHERE (a, b) NOT IN (VALUES (1, 'one'), (3, 'three')) ORDER BY a",
             [["2"]], None, None, None, None, None,
             "row-wise NOT IN (VALUES ...)"),
            ("SELECT 1 WHERE 1 IN (VALUES (1, 2))",
             None, None, "42601", "expected 1", None, None,
             "scalar IN with a 2-column VALUES row: 42601"),
            ("SELECT 1 WHERE (1, 2) IN (VALUES (1, 2, 3))",
             None, None, "42601", "expected 2", None, None,
             "row-wise IN with mismatched column count: 42601"),
        ]

        for sql, wrows, wtag, wcode, wmsg, woids, wcols, desc in cases:
            rows, tag, err, code, oids, cols = run_sql(s, sql)
            ok = True
            why = ""
            if wcode is not None:
                if code != wcode:
                    ok = False
                    why = f"want code {wcode}, got {code} ({err})"
                elif wmsg and wmsg not in (err or ""):
                    ok = False
                    why = f"want msg containing {wmsg!r}, got {err!r}"
            else:
                if code is not None:
                    ok = False
                    why = f"unexpected error {code}: {err}"
                elif wrows is not None and rows != wrows:
                    ok = False
                    why = f"want rows {wrows}, got {rows}"
                elif wtag is not None and tag != wtag:
                    ok = False
                    why = f"want tag {wtag!r}, got {tag!r}"
                elif woids is not None and oids != woids:
                    ok = False
                    why = f"want oids {woids}, got {oids}"
                elif wcols is not None and cols != wcols:
                    ok = False
                    why = f"want cols {wcols}, got {cols}"
            if ok:
                passed += 1
                print(f"ok: {desc}")
            else:
                failed += 1
                print(f"FAIL: {desc}\n  sql: {sql}\n  {why}")
        s.close()
        print(f"\n{passed} pass / {failed} fail")
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
