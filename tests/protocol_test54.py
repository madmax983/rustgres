#!/usr/bin/env python3
"""Protocol 54: unary minus as first-class Expr::Neg + prefix-operator precedence.

v0.53 implements two PG19 parser/executor behaviors, grounded in
REL_19_STABLE gram.y and the executor's doNegate:

* Unary `-`/`+` sit at UMINUS precedence (gram.y): tighter than `^`
  (so `-2^2` is `(-2)^2` = 4) but looser than `::` (so `-5::int` is
  `-(5::int)`). `-x` is a first-class `Expr::Neg` (PG's doNegate):
  type-preserving (`-smallint` stays smallint — the old `0 - x`
  desugar widened it to integer), overflow raises 22003 with the
  per-type message (`-(-32768::smallint)` errors; the desugar
  returned 32768), NULL stays NULL, and unknown-type text literals
  keep the old `0 - x` resolution path (`-'5'` is -5, `-'2026-01-01'`
  still fails with 22P02, like Postgres).
* The generic prefix operators `~`, `@`, `|/`, `||/` are PG19's
  `qual_Op a_expr %prec Op` — the loosest precedence in the grammar —
  so their operand is a full comparison-level expression: `~ 1 + 1`
  is `~(1 + 1)` = -3 (was -1), `@ 5 - 10` is `@(5 - 10)` = 5
  (was -5), `|/ 2 + 7` is `|/(2 + 7)` = 3, `||/ 35 - 8` is
  `||/(35 - 8)` = 3. `~ 5::int2` is `~(5::int2)`: integer -6
  (was smallint -6 — PG has no int2 `~`, the int4 promotion is
  correct). `~ NULL` is NULL::int (was 42883).

RED on v0.52 (base 0bd75c5): the precedence cases produce the wrong
values/types, `-(-32768::smallint)` wrongly returns 32768, `-s` on a
smallint column reports OID 23, and `~ NULL` raises 42883.
GREEN on v0.53: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5554
DATA_DIR = "/tmp/rg54proto"
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
    "CREATE TABLE neg54 (s smallint, i int, b bigint)",
]


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg54proto.log", "w")
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
        # OIDs: 21=int2, 23=int4, 20=int8, 1700=numeric
        cases = [
            # --- prefix operators bind loosest (PG19 qual_Op a_expr %prec Op) ---
            ("SELECT ~ 1 + 1",
             [["-3"]], None, None, None, [23], None,
             "~ binds loosest: ~(1+1) = -3 (v0.52 gave -1)"),
            ("SELECT ~ 5::int2",
             [["-6"]], None, None, None, [23], None,
             "~(5::int2): int4 -6 (v0.52 gave smallint -6)"),
            ("SELECT @ 5 - 10",
             [["5"]], None, None, None, [23], None,
             "@ binds loosest: @(5-10) = 5 (v0.52 gave -5)"),
            ("SELECT |/ 2 + 7",
             [["3"]], None, None, None, [1700], None,
             "|/ binds loosest: |/(2+7) = 3"),
            ("SELECT ||/ 35 - 8",
             [["3.000000000000000"]], None, None, None, [1700], None,
             "||/ binds loosest: ||/(35-8) = 3 (v0.61: PG cbrt dscale 15)"),
            # --- Expr::Neg: type-preserving doNegate ---
            ("SELECT - (-32768::smallint)",
             None, None, "22003", "smallint out of range", None, None,
             "doNegate overflow: 22003 (v0.52 returned 32768)"),
            ("SELECT -s, -i, -b FROM neg54",
             [], None, None, None, [21, 23, 20], None,
             "-smallint stays smallint (v0.52 widened to int4)"),
            ("SELECT - 30000::smallint",
             [["-30000"]], None, None, None, [21], None,
             "-(30000::int2): smallint"),
            ("SELECT - 5::int2",
             [["-5"]], None, None, None, [21], None,
             "-(5::int2): smallint"),
            # --- ~ NULL: PG resolves unknown NULL to int4 ---
            ("SELECT ~ NULL",
             [[None]], None, None, None, [23], None,
             "~ NULL is NULL::int (v0.52 raised 42883)"),
            # --- preserved behavior (pass on both) ---
            ("SELECT - 2 ^ 2",
             [["4.0000000000000000"]], None, None, None, [1700], None,
             "UMINUS tighter than ^: (-2)^2 = 4 (v0.61: PG power rscale 16)"),
            ("SELECT - NULL",
             [[None]], None, None, None, [23], None,
             "- NULL is NULL"),
            ("SELECT - -5",
             [["5"]], None, None, None, [23], None,
             "double negation"),
            ("SELECT -(3+4)",
             [["-7"]], None, None, None, [23], None,
             "negation of a parenthesized sum"),
            ("SELECT ~ 7::bigint",
             [["-8"]], None, None, None, [20], None,
             "~(7::int8): bigint -8"),
            ("SELECT - 30000",
             [["-30000"]], None, None, None, [23], None,
             "plain -int stays int"),
            ("SELECT -'2026-01-01'",
             None, None, "22P02", "invalid input syntax", None, None,
             "unknown-literal path preserved: 22P02 like Postgres"),
            ("SELECT -'5'",
             [["-5"]], None, None, None, [23], None,
             "-'5' resolves to -5 like the old 0 - x path"),
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
        print(f"protocol 54: {passed} passed, {failed} failed")
        s.close()
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
