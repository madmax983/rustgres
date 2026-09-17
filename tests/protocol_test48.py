#!/usr/bin/env python3
"""Protocol 48: SELECT-list set-returning functions (generate_series).

v0.47 implements PG19's SRF-in-targetlist semantics (planner.c
`adjust_paths_for_srfs` / nodeProjectSet.c `ExecProjectSRF`): a
top-level set-returning function call in the SELECT list fans each
input row out to one row per returned element; plain columns repeat;
multiple SRFs fan out to the max width with the exhausted ones padded
NULL; an all-empty SRF set yields zero rows.

v0.47 also resolves GROUP BY ordinals to select-list expressions
(PG19 parse analysis: `GROUP BY 1` groups by the first select item),
and expands SRFs in the GROUP BY keys below the aggregate
(PG19 ProjectSet under Agg), so aggregates fold the expanded rows.

Wire types follow the FROM-clause rules: int4 -> OID 23, int8 -> 20,
numeric -> 1700.

RED on v0.46: `SELECT generate_series(1,3)` is 42883 (the function only
existed as a FROM-clause table function); GROUP BY ordinals grouped by
a constant.
GREEN on v0.47: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5548
DATA_DIR = "/tmp/rg48proto"
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
    return rows, err, errcode, oids, cols


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg48proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        # Fixture for the GROUP BY cases (small tenk1 analog).
        for setup in [
            "CREATE TABLE gs_ten(ten int)",
            "INSERT INTO gs_ten VALUES (0), (1), (2), (3)",
        ]:
            rows, err, code, _, _ = run_sql(s, setup)
            if code is not None:
                print(f"SETUP FAILED: {setup}: {code} {err}")
                raise SystemExit(2)

        # (sql, expected_rows_or_None, expected_errcode_or_None,
        #  expected_oids_or_None, expected_colnames_or_None, description)
        cases = [
            # --- the headline case
            ("SELECT generate_series(1,3)",
             [["1"], ["2"], ["3"]], None, [23], ["generate_series"],
             "single SRF fans one input row out to 3 rows"),
            ("SELECT generate_series(1,3) AS g",
             [["1"], ["2"], ["3"]], None, [23], ["g"],
             "alias renames the SRF column"),
            # --- plain columns repeat; steps; empty
            ("SELECT 1 AS t, generate_series(1,3) AS x",
             [["1", "1"], ["1", "2"], ["1", "3"]], None, [23, 23], ["t", "x"],
             "plain select-list columns repeat on every fanned-out row"),
            ("SELECT generate_series(1,5,2)",
             [["1"], ["3"], ["5"]], None, [23], None,
             "positive step"),
            ("SELECT generate_series(3,1,-1)",
             [["3"], ["2"], ["1"]], None, [23], None,
             "negative step"),
            ("SELECT generate_series(5,1)",
             [], None, [23], None,
             "positive step, start > stop -> no rows"),
            ("SELECT 1 AS t, generate_series(1,0) AS x",
             [], None, [23, 23], None,
             "all-empty SRF set yields zero rows even next to plain columns"),
            ("SELECT generate_series(NULL,3)",
             [], None, [23], None,
             "strict: NULL input yields zero rows"),
            # --- multiple SRFs: max width, exhausted pads NULL
            ("SELECT generate_series(1,2), generate_series(10,12)",
             [["1", "10"], ["2", "11"], [None, "12"]], None, [23, 23], None,
             "two SRFs fan to max width; exhausted SRF pads NULL"),
            # --- types
            ("SELECT generate_series(1::bigint, 3::bigint)",
             [["1"], ["2"], ["3"]], None, [20], None,
             "int8 SRF, OID 20"),
            ("SELECT generate_series(1.5, 2.5)",
             [["1.5"], ["2.5"]], None, [1700], None,
             "numeric SRF, OID 1700"),
            ("SELECT generate_series(0.1::numeric, 4.0::numeric, 1.3::numeric)",
             [["0.1"], ["1.4"], ["2.7"], ["4"]], None, [1700], None,
             "numeric series, minimal-scale rendering"),
            # --- errors
            ("SELECT generate_series(1,3,0)",
             None, "22023", None, None,
             "step zero -> 22023"),
            ("SELECT generate_series(1)",
             None, "42883", None, None,
             "wrong arity -> no such function"),
            ("SELECT generate_series('a','z')",
             None, "42883", None, None,
             "bad arg types -> no such function"),
            # --- SRF in a UNION branch (conformance union.sql)
            ("SELECT * FROM (SELECT 1 AS t, generate_series(1,10) AS x "
             "UNION SELECT 2 AS t, 4 AS x) ss WHERE x < 4 ORDER BY x",
             [["1", "1"], ["1", "2"], ["1", "3"]], None, [23, 23], ["t", "x"],
             "SRF expands inside a UNION branch"),
            # --- correlated SRF args per input row
            ("SELECT ten, generate_series(1, ten) AS g FROM gs_ten WHERE ten <= 2 ORDER BY 1, 2",
             [["1", "1"], ["2", "1"], ["2", "2"]], None, [23, 23], None,
             "correlated SRF args re-evaluated per row; empty set drops the row"),
            # --- GROUP BY ordinals
            ("SELECT ten, count(*) FROM gs_ten GROUP BY 1 ORDER BY 1",
             [["0", "1"], ["1", "1"], ["2", "1"], ["3", "1"]], None, [23, 20], None,
             "GROUP BY 1 groups by the first select item"),
            ("SELECT ten FROM gs_ten GROUP BY 5",
             None, "42803", None, None,
             "out-of-range GROUP BY ordinal -> 42803"),
            # --- SRF in GROUP BY expands below the aggregate
            # (conformance subselect.sql shape; expansions: 0->{}, 1->{1},
            #  2->{1,2}, 3->{1,2,3}, so g=1:3 rows, g=2:2 rows, g=3:1 row)
            ("SELECT generate_series(1, ten) AS g, count(*) FROM gs_ten GROUP BY 1 ORDER BY 1",
             [["1", "3"], ["2", "2"], ["3", "1"]], None, [23, 20], ["g", "count"],
             "SRF group key expands per input row; aggregates fold expanded rows"),
            # --- SRF under LIMIT / DISTINCT / subquery
            ("SELECT generate_series(1,5) LIMIT 2",
             [["1"], ["2"]], None, [23], None,
             "LIMIT applies after SRF expansion"),
            ("SELECT DISTINCT generate_series(1,2)",
             [["1"], ["2"]], None, [23], None,
             "DISTINCT over expanded rows"),
            ("SELECT count(*) FROM (SELECT generate_series(1,2) FROM gs_ten) s",
             [["8"]], None, [20], None,
             "SRF expansion inside a derived table (4 input rows x 2)"),
            # --- FROM-clause table function still works
            ("SELECT * FROM generate_series(1,2)",
             [["1"], ["2"]], None, [23], None,
             "FROM-clause generate_series unaffected"),
        ]

        for sql, want_rows, want_code, want_oids, want_cols, desc in cases:
            rows, err, code, oids, cols = run_sql(s, sql)
            ok = True
            detail = ""
            if want_code is not None:
                if code != want_code:
                    ok = False
                    detail = f"want err {want_code}, got {code} ({err})"
            else:
                if code is not None:
                    ok = False
                    detail = f"unexpected err {code} ({err})"
                elif want_rows is not None and rows != want_rows:
                    ok = False
                    detail = f"want rows {want_rows}, got {rows}"
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
        print(f"protocol 48: {passed} passed, {failed} failed")
        s.close()
    finally:
        proc.terminate()

    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
