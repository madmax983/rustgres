#!/usr/bin/env python3
"""Protocol 53: SELECT DISTINCT ON (PG19-exact).

v0.52 implements SELECT DISTINCT ON, grounded in PG19 REL_19_STABLE
(gram.y DistinctClause, parse analysis, and the executor's Unique node):

* `SELECT DISTINCT ON (expr [, ...]) ...` keeps the first row of each
  group of rows where the DISTINCT ON expressions are equal (NULLs
  group together). PG19 always sorts by the effective ORDER BY —
  distinct keys first, then the tail — even with no user ORDER BY, and
  the first row per group wins.
* The DISTINCT ON prefix rule is PG19's set-prefix rule (42803
  "SELECT DISTINCT ON expressions must match initial ORDER BY
  expressions"): ORDER BY may permute the keys and may name fewer keys
  than DISTINCT ON (missing keys are appended as implicit ASC NULLS
  LAST); a key after a non-key term, or an unreached key with a non-key
  tail, is 42803. Ordinals (incl. over `*`), output aliases, and
  qualified-vs-unqualified columns resolve like PG's parse analysis.
* DISTINCT ON supports GROUP BY and aggregates (the filter runs above
  the grouping); aggregates and window functions are legal inside the
  DISTINCT ON expressions themselves.
* ORDER BY may name non-projected columns (no 42703, unlike plain
  DISTINCT).
* SRF-in-targetlist fans out AFTER unique-ification (PG's ProjectSet
  sits above Unique).
* A comma after the DISTINCT ON clause (`DISTINCT ON (a), b`) is a
  syntax error (42601) — real PostgreSQL rejects it, despite the
  assignment brief's claim; this implementation follows PostgreSQL.

RED on v0.51: every DISTINCT ON statement fails to parse (42601), so
all cases fail.
GREEN on v0.52: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5553
DATA_DIR = "/tmp/rg53proto"
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
    body = struct.pack("!i", 196608) + b"user\\x00postgres\\x00\\x00"
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
    "CREATE TABLE don (a int, b int)",
    "INSERT INTO don VALUES (1,10),(1,20),(1,30),(2,5),(2,15),(NULL,7),(NULL,3)",
]


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg53proto.log", "w")
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
        cases = [
            # --- basics: first row per group wins the ORDER BY ---
            ("SELECT DISTINCT ON (a) a, b FROM don ORDER BY a, b",
             [["1", "10"], ["2", "5"], [None, "3"]], None, None, None, None, None,
             "one row per a: smallest b (NULLs group, NULL b=3 wins)"),
            ("SELECT DISTINCT ON (a) a, b FROM don ORDER BY a, b DESC",
             [["1", "30"], ["2", "15"], [None, "7"]], None, None, None, None, None,
             "DESC: largest b per group wins"),
            ("SELECT DISTINCT ON (a, b) a, b FROM don ORDER BY a, b",
             [["1", "10"], ["1", "20"], ["1", "30"], ["2", "5"], ["2", "15"],
              [None, "3"], [None, "7"]], None, None, None, None, None,
             "two distinct exprs: every row its own group"),
            # --- DISTINCT ON expr not projected; ORDER BY on non-projected col ---
            ("SELECT DISTINCT ON (a) b FROM don ORDER BY a, b",
             [["10"], ["5"], ["3"]], None, None, None, None, None,
             "non-projected distinct expr + non-projected ORDER BY col (no 42703)"),
            # --- ordinals and aliases resolve for the 42803 prefix check ---
            ("SELECT DISTINCT ON (a) a, b FROM don ORDER BY 1, 2",
             [["1", "10"], ["2", "5"], [None, "3"]], None, None, None, None, None,
             "ORDER BY ordinals satisfy the prefix check"),
            ("SELECT DISTINCT ON (a) a AS x, b FROM don ORDER BY x, b",
             [["1", "10"], ["2", "5"], [None, "3"]], None, None, None, None, None,
             "ORDER BY output alias satisfies the prefix check"),
            # --- expression keys ---
            ("SELECT DISTINCT ON (b % 2) b FROM don ORDER BY b % 2, b",
             [["10"], ["3"]], None, None, None, None, None,
             "expression keys: even bucket -> 10, odd bucket -> 3"),
            # --- no ORDER BY: PG still sorts by the distinct keys ---
            ("SELECT DISTINCT ON (a) a FROM don",
             [["1"], ["2"], [None]], None, None, None, None, None,
             "no ORDER BY: one row per group key, sorted by the key"),
            # --- LIMIT applies after the filter ---
            ("SELECT DISTINCT ON (a) a, b FROM don ORDER BY a, b LIMIT 2",
             [["1", "10"], ["2", "5"]], None, None, None, None, None,
             "LIMIT 2 after first-row-per-group"),
            # --- through a derived table (corpus shape) ---
            ("SELECT * FROM (SELECT DISTINCT ON (a) a, b FROM don ORDER BY a, b) s ORDER BY 1, 2",
             [["1", "10"], ["2", "5"], [None, "3"]], None, None, None, None, None,
             "DISTINCT ON inside a derived table"),
            # --- SRF fans out AFTER unique-ification (ProjectSet above Unique) ---
            ("SELECT DISTINCT ON (a) a, generate_series(1, 2) AS g FROM don ORDER BY a",
             [["1", "1"], ["1", "2"], ["2", "1"], ["2", "2"], [None, "1"], [None, "2"]],
             None, None, None, None, None,
             "SRF expands per surviving row (3 groups x 2)"),
            # --- 42803: PG19 set-prefix rule ---
            ("SELECT DISTINCT ON (a) a, b FROM don ORDER BY b, a",
             None, None, "42803", "must match initial ORDER BY expressions", None, None,
             "42803: key after a non-key term"),
            ("SELECT DISTINCT ON (a, b) a, b FROM don ORDER BY a, b + 100",
             None, None, "42803", "must match initial ORDER BY expressions", None, None,
             "42803: unreached key with a non-key tail"),
            ("SELECT DISTINCT ON (a, b) a, b FROM don ORDER BY b, a",
             [[None, "3"], ["2", "5"], [None, "7"], ["1", "10"], ["2", "15"],
              ["1", "20"], ["1", "30"]], None, None, None, None, None,
             "permuted keys satisfy the prefix rule"),
            ("SELECT DISTINCT ON (a, b) a, b FROM don ORDER BY a",
             [["1", "10"], ["1", "20"], ["1", "30"], ["2", "5"], ["2", "15"],
              [None, "3"], [None, "7"]], None, None, None, None, None,
             "short prefix: missing key appended implicitly (ASC)"),
            # --- GROUP BY / aggregates (PG19 allows them) ---
            ("SELECT DISTINCT ON (a) a, count(*) FROM don GROUP BY a ORDER BY a",
             [["1", "3"], ["2", "2"], [None, "2"]], None, None, None, None, None,
             "DISTINCT ON over groups"),
            ("SELECT DISTINCT ON (count(*)) a, count(*) FROM don GROUP BY a ORDER BY count(*), a",
             [["2", "2"], ["1", "3"]], None, None, None, None, None,
             "aggregate DISTINCT ON key (tie broken by a)"),
            # --- window function in the DISTINCT ON key ---
            ("SELECT DISTINCT ON (rank() OVER (ORDER BY b)) a, b FROM don ORDER BY rank() OVER (ORDER BY b), a",
             [[None, "3"], ["2", "5"], [None, "7"], ["1", "10"], ["2", "15"],
              ["1", "20"], ["1", "30"]], None, None, None, None, None,
             "window-function DISTINCT ON key"),
            # --- 42601: parens are mandatory; no comma after the ON clause ---
            ("SELECT DISTINCT ON a, b FROM don",
             None, None, "42601", None, None, None,
             "42601: DISTINCT ON without parens"),
            ("SELECT DISTINCT ON (a), b FROM don ORDER BY a, b",
             None, None, "42601", None, None, None,
             "42601: comma after the DISTINCT ON clause (PG rejects)"),
        ]

        for sql, want_rows, want_tag, want_code, want_msg, want_oids, want_cols, desc in cases:
            rows, tag, err, code, oids, cols = run_sql(s, sql)
            ok = True
            detail = ""
            if want_code is not None:
                if code != want_code:
                    ok = False
                    detail = f"want err {want_code}, got {code} ({err})"
                elif want_msg is not None and want_msg not in (err or ""):
                    ok = False
                    detail = f"want msg containing {want_msg!r}, got {err!r}"
            else:
                if code is not None:
                    ok = False
                    detail = f"unexpected err {code} ({err})"
                elif want_rows is not None and rows != want_rows:
                    ok = False
                    detail = f"want rows {want_rows}, got {rows}"
                elif want_tag is not None and tag != want_tag:
                    ok = False
                    detail = f"want tag {want_tag}, got {tag}"
                elif want_oids is not None and oids != want_oids:
                    ok = False
                    detail = f"want OIDs {want_oids}, got {oids}"
                elif want_cols is not None and cols != want_cols:
                    ok = False
                    detail = f"want cols {want_cols}, got {cols}"
            if ok:
                passed += 1
            else:
                failed += 1
                print(f"FAIL: {desc}\n  sql: {sql}\n  {detail}")
        print(f"protocol 53: {passed} passed, {failed} failed")
        s.close()
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
