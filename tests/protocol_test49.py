#!/usr/bin/env python3
"""Protocol 49: IS [NOT] DISTINCT FROM + CREATE TABLE AS SELECT (CTAS).

v0.48 implements PG19's NULL-safe comparison (gram.y): `a IS [NOT]
DISTINCT FROM b` never returns unknown — NULLs compare equal to NULL,
NaN compares equal to NaN, otherwise it is the negation of `=`.
The parser accepts the IS [NOT] DISTINCT FROM form at the same
precedence level as the other IS forms; both the row and grouped
(HAVING) evaluators share one value-level implementation.

v0.48 also implements CREATE TABLE AS SELECT (PG19 createas.c /
intorel.c): the query executes before creation, output names/types are
inferred, optional column aliases apply, temp/permanent tables land in
the right namespace, rows insert with row IDs/TOAST/indexes, the tag is
PostgreSQL-style `SELECT n`, and WITH [NO] DATA plus IF NOT EXISTS are
honored. (Honest deviation: WITH NO DATA executes the query and
discards its rows; PostgreSQL avoids executing it.)

RED on v0.47: `IS DISTINCT FROM` is 42601 and CTAS is 42601 (both
unparsed).
GREEN on v0.48: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5549
DATA_DIR = "/tmp/rg49proto"
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


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg49proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        for setup in [
            "CREATE TEMP TABLE disttable (f1 integer)",
            "INSERT INTO disttable VALUES(1)",
            "INSERT INTO disttable VALUES(2)",
            "INSERT INTO disttable VALUES(3)",
            "INSERT INTO disttable VALUES(NULL)",
        ]:
            rows, tag, err, code, _, _ = run_sql(s, setup)
            if code is not None:
                print(f"SETUP FAILED: {setup}: {code} {err}")
                raise SystemExit(2)

        # (sql, want_rows_or_None, want_tag_or_None, want_code_or_None,
        #  want_oids_or_None, want_cols_or_None, description)
        cases = [
            # --- the PG19 select_distinct corpus block
            ('SELECT f1, f1 IS DISTINCT FROM 2 as "not 2" FROM disttable',
             [["1", "t"], ["2", "f"], ["3", "t"], [None, "t"]], None, None, [23, 16], None,
             "IS DISTINCT FROM: NULL not distinct from NULL, rest by ="),
            ('SELECT f1, f1 IS DISTINCT FROM NULL as "not null" FROM disttable',
             [["1", "t"], ["2", "t"], ["3", "t"], [None, "f"]], None, None, None, None,
             "IS DISTINCT FROM NULL: only NULL is not distinct"),
            ('SELECT f1, f1 IS DISTINCT FROM f1 as "false" FROM disttable',
             [["1", "f"], ["2", "f"], ["3", "f"], [None, "f"]], None, None, None, None,
             "self-compare is never distinct, even NULL"),
            ('SELECT f1, f1 IS DISTINCT FROM f1+1 as "not null" FROM disttable',
             [["1", "t"], ["2", "t"], ["3", "t"], [None, "f"]], None, None, None, None,
             "NULL+1 is NULL, not distinct from NULL"),
            ('SELECT 1 IS DISTINCT FROM 2 as "yes"',
             [["t"]], None, None, [16], ["yes"],
             "booleans render t/f, wire OID 16"),
            ('SELECT 2 IS DISTINCT FROM 2 as "no"',
             [["f"]], None, None, None, None,
             "equal values not distinct"),
            ('SELECT 2 IS DISTINCT FROM null as "yes"',
             [["t"]], None, None, None, None,
             "value vs NULL is distinct"),
            ('SELECT null IS DISTINCT FROM null as "no"',
             [["f"]], None, None, None, None,
             "NULL vs NULL is not distinct"),
            ('SELECT 1 IS NOT DISTINCT FROM 2 as "no"',
             [["f"]], None, None, None, None,
             "IS NOT DISTINCT FROM negates"),
            ('SELECT 2 IS NOT DISTINCT FROM 2 as "yes"',
             [["t"]], None, None, None, None,
             "IS NOT DISTINCT FROM: equal values"),
            ('SELECT 2 IS NOT DISTINCT FROM null as "no"',
             [["f"]], None, None, None, None,
             "IS NOT DISTINCT FROM: value vs NULL"),
            ('SELECT null IS NOT DISTINCT FROM null as "yes"',
             [["t"]], None, None, None, None,
             "IS NOT DISTINCT FROM: NULL vs NULL"),
            # --- extras: cross-type, NaN, text, WHERE, grouped
            ("SELECT 1 IS DISTINCT FROM 1.0",
             [["f"]], None, None, None, None,
             "cross-type equality still holds"),
            ("SELECT 'a' IS DISTINCT FROM 'b'",
             [["t"]], None, None, None, None,
             "text comparison"),
            ("SELECT 'nan'::numeric IS DISTINCT FROM 'nan'::numeric",
             [["f"]], None, None, None, None,
             "numeric NaN equal to NaN (unlike =)"),
            ("SELECT 'nan'::float8 IS DISTINCT FROM 'nan'::float8",
             [["f"]], None, None, None, None,
             "float8 NaN equal to NaN"),
            ("SELECT f1 FROM disttable WHERE f1 IS DISTINCT FROM 2",
             [["1"], ["3"], [None]], None, None, None, None,
             "IS DISTINCT FROM in WHERE keeps NULL rows"),
            ("SELECT 1 IS DISTINCT FROM NULL",
             [["t"]], None, None, None, ["?column?"],
             "expression column named ?column? like PG"),
            # --- the v0.48 CTAS headline queries (PG19 select_distinct)
            ("CREATE TABLE distinct_group_1 AS SELECT DISTINCT g%1000 FROM generate_series(0,9999) g",
             [], "SELECT 1000", None, None, None,
             "CTAS headline: SELECT 1000"),
            ("SELECT count(*) FROM distinct_group_1",
             [["1000"]], None, None, None, None,
             "CTAS table holds 1000 distinct rows"),
            ("CREATE TABLE distinct_group_2 AS SELECT DISTINCT (g%1000)::text FROM generate_series(0,9999) g",
             [], "SELECT 1000", None, None, None,
             "CTAS text-cast variant: SELECT 1000"),
            ("CREATE TABLE distinct_hash_1 AS SELECT DISTINCT g%1000 FROM generate_series(0,9999) g",
             [], "SELECT 1000", None, None, None,
             "CTAS hash variant 1: SELECT 1000"),
            ("CREATE TABLE distinct_hash_2 AS SELECT DISTINCT (g%1000)::text FROM generate_series(0,9999) g",
             [], "SELECT 1000", None, None, None,
             "CTAS hash variant 2: SELECT 1000"),
            ("CREATE TABLE ctas_alias (x, y) AS SELECT 1+1, 2+2",
             [], "SELECT 1", None, None, None,
             "CTAS column aliases"),
            ("SELECT x, y FROM ctas_alias",
             [["2", "4"]], None, None, None, ["x", "y"],
             "CTAS aliases visible"),
            ("CREATE TEMP TABLE ctas_temp AS SELECT 42 AS a",
             [], "SELECT 1", None, None, None,
             "CTAS temp table"),
            ("CREATE TABLE ctas_nodata AS SELECT 1 AS a WITH NO DATA",
             [], "SELECT 0", None, None, None,
             "CTAS WITH NO DATA: empty, tag SELECT 0"),
            ("CREATE TABLE IF NOT EXISTS ctas_nodata AS SELECT 1 AS a",
             [], None, None, None, None,
             "CTAS IF NOT EXISTS on existing table is a no-op notice"),
            ("CREATE TABLE ctas_dup AS SELECT 1",
             [], "SELECT 1", None, None, None,
             "CTAS base table"),
            ("CREATE TABLE ctas_dup AS SELECT 1",
             None, None, "42P07", None, None,
             "CTAS duplicate name -> 42P07"),
            # --- read-only transaction enforcement (PG19)
            ("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
             [], "SET", None, None, None,
             "enter read-only mode"),
            ("CREATE TABLE ctas_ro AS SELECT 1 AS a",
             None, None, "25006", None, None,
             "CTAS in a read-only transaction -> 25006"),
            ("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
             [], "SET", None, None, None,
             "back to read-write"),
            ("CREATE TABLE ctas_rw AS SELECT 1 AS a",
             [], "SELECT 1", None, None, None,
             "CTAS works again read-write"),
        ]

        for sql, want_rows, want_tag, want_code, want_oids, want_cols, desc in cases:
            rows, tag, err, code, oids, cols = run_sql(s, sql)
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
        print(f"protocol 49: {passed} passed, {failed} failed")
        s.close()
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
