#!/usr/bin/env python3
"""Protocol 50: parenthesized expressions as the first select-list item +
quoted `"char"` casts after parentheses.

v0.49 fixes the v0.44 `parse_set_branch` regression: after the SELECT
keyword PG19's gram.y allows only a target_list, so a `(` always opens a
parenthesized *expression* (or scalar subquery), never a parenthesized
set branch. The old code parsed `SELECT (-1)` as a query and died with
"expected SELECT, found Minus" (42601); it also silently unwrapped
`SELECT (SELECT ...)` scalar subqueries, losing the 21000 multi-row
check. The quoted `"char"` type name itself (OID 18, distinct from
`character(1)`) already resolved in `::` casts since v0.36 — the only
failing `"char"` case was the parenthesized one, `(-1)::"char"`.

RED on v0.48: every parenthesized-first-item case is 42601
(unparsed); `SELECT (SELECT id FROM users)` wrongly returns rows.
GREEN on v0.49: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5550
DATA_DIR = "/tmp/rg50proto"
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
    log = open("/tmp/rg50proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        for setup in [
            "CREATE TEMP TABLE p50_users (id integer, name text)",
            "INSERT INTO p50_users VALUES (1, 'ann'), (2, 'bob')",
        ]:
            rows, tag, err, code, _, _ = run_sql(s, setup)
            if code is not None:
                print(f"SETUP FAILED: {setup}: {code} {err}")
                raise SystemExit(2)

        # (sql, want_rows_or_None, want_tag_or_None, want_code_or_None,
        #  want_oids_or_None, want_cols_or_None, description)
        cases = [
            # --- headline: quoted "char" casts, incl. parenthesized operand
            ('SELECT (-1)::"char"',
             [["\\377"]], None, None, [18], ["char"],
             "B9: (-1)::\"char\" is i4tochar(-1) = byte 255, OID 18"),
            ('SELECT \'a\'::"char"',
             [["a"]], None, None, [18], ["char"],
             "'a'::\"char\" still works"),
            ('SELECT 65::"char"',
             [["A"]], None, None, [18], ["char"],
             "65::\"char\" still works"),
            ('SELECT CAST((-1) AS "char")',
             [["\\377"]], None, None, [18], ["char"],
             "CAST((-1) AS \"char\") quoted type in CAST form"),
            ('SELECT \'a\'::"char"::text',
             [["a"]], None, None, [25], None,
             "chained cast through \"char\""),
            ('SELECT 200::"char"',
             None, None, "22003", None, None,
             "int->\"char\" out of range still 22003"),
            # --- parenthesized expressions as first select-list item
            ("SELECT (-1)",
             [["-1"]], None, None, None, None,
             "parenthesized unary minus parses as expression"),
            ("SELECT (1+2)",
             [["3"]], None, None, None, None,
             "parenthesized arithmetic parses as expression"),
            ("SELECT (-1)::int",
             [["-1"]], None, None, [23], None,
             "parenthesized operand with unquoted cast"),
            ("SELECT (1+2)::int",
             [["3"]], None, None, [23], None,
             "parenthesized arithmetic with cast"),
            ("SELECT 1, (-1)::int",
             [["1", "-1"]], None, None, None, None,
             "parenthesized operand in non-first position"),
            ("SELECT ((-1))",
             [["-1"]], None, None, None, None,
             "doubly-parenthesized expression"),
            # --- scalar subqueries stay subqueries (not unwrapped branches)
            ("SELECT (SELECT 1)",
             [["1"]], None, None, None, None,
             "scalar subquery as first select item"),
            ("SELECT (SELECT id FROM p50_users WHERE id = 2)",
             [["2"]], None, None, None, None,
             "single-row scalar subquery evaluates"),
            ("SELECT (SELECT id FROM p50_users)",
             None, None, "21000", None, None,
             "multi-row scalar subquery raises 21000"),
            ("SELECT (SELECT name FROM p50_users WHERE id = 99) IS NULL AS e",
             [["t"]], None, None, None, ["e"],
             "empty scalar subquery is NULL"),
            # --- set operations still work around parentheses
            ("(SELECT 1 UNION SELECT 2) UNION SELECT 3",
             [["1"], ["2"], ["3"]], None, None, None, None,
             "top-level parenthesized set branch still works"),
            ("SELECT 1 UNION (SELECT 2)",
             [["1"], ["2"]], None, None, None, None,
             "parenthesized right set operand still works"),
            ("SELECT * FROM (SELECT 1 AS x UNION SELECT 2 AS x) t ORDER BY x",
             [["1"], ["2"]], None, None, None, None,
             "derived-table parenthesized set query still works"),
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
        print(f"protocol 50: {passed} passed, {failed} failed")
        s.close()
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
