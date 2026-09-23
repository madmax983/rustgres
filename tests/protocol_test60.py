#!/usr/bin/env python3
"""Protocol 60: PG19 numeric division + CTE VALUES + numeric edges.

v0.59 ships PostgreSQL 19's numeric division semantics and standalone
VALUES in CTE bodies, plus small numeric-input edges:

* `/` uses PG19 `select_div_scale` (at least 16 significant digits,
  not less than either input's display scale) with exact BigUint long
  division and round-half-away-from-zero on the guard digit. E.g.
  `999999999999999999999 / 1000000000000000000000` = 1 (was
  0.9999999999 at scale 10) and `12345678901234567890 / 123` =
  100371373180768845 (was ...844.6341463414).
* `div()` computes the quotient directly at scale 0 with truncation
  (PG19 calls div_var with rscale 0 and no rounding).
* `round(x, s)` with s >= x's scale is a value-identical no-op (was
  22003 when zero-padding overflowed i128, e.g. round(3.14, 40)).
* `WITH v(x) AS (VALUES (1),(2))` CTE bodies parse; VALUES cells coerce
  to the resolved column type (unknown literals take it, like PG).
* `scale()`/`min_scale()` return NULL for NaN/Infinity (PG19
  numeric_scale/numeric_min_scale); min_scale clamps at 0.
* `numeric(p, s)` DDL accepts a negative scale (PG19 typmod).
* `_` digit separators follow PG19 set_var_from_str: `_123`,
  `123._456`, `1.2e_34` are 22P02; `1_2` is fine.

RED on v0.58 (base ce3bc192): division values wrong, round(3.14,40)
22003, CTE VALUES 42601, scale('NaN') = 0, min_scale(1e100) = -100,
numeric(3,-6) 42601, '_123'::numeric accepted.
GREEN on v0.59: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5560
DATA_DIR = "/tmp/rg60proto"
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
    log = open("/tmp/rg60proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        # (sql, want_rows_or_None, want_tag_or_None, want_code_or_None,
        #  want_msg_contains_or_None, want_oids_or_None, want_cols_or_None,
        #  description)
        # OIDs: 1700=numeric, 23=int4
        cases = [
            # --- PG19 division: select_div_scale + exact rounding ---
            ("SELECT 999999999999999999999::numeric / 1000000000000000000000::numeric;",
             [["1.00000000000000000000"]], None, None, None, [1700], None,
             "21-nines / 1e21 rounds to 1 at scale 20 (v0.61: PG displays rscale)"),
            ("SELECT 12345678901234567890::numeric / 123::numeric;",
             [["100371373180768845"]], None, None, None, [1700], None,
             "big quotient at scale 0 rounds half away (was ...844.6341463414)"),
            ("SELECT 1::numeric / 3::numeric;",
             [["0.33333333333333333333"]], None, None, None, [1700], None,
             "1/3 carries 20 threes (16 sig digits)"),
            ("SELECT 10::numeric / 3::numeric;",
             [["3.3333333333333333"]], None, None, None, [1700], None,
             "10/3 carries 16 threes"),
            ("SELECT 22::numeric / 7::numeric;",
             [["3.1428571428571429"]], None, None, None, [1700], None,
             "22/7 = 3.1428571428571429"),
            # --- div(): truncation at scale 0, no rounding ---
            ("SELECT div(7::numeric, 2::numeric);",
             [["3"]], None, None, None, [1700], None,
             "div(7,2) = 3"),
            ("SELECT div(-7::numeric, 2::numeric);",
             [["-3"]], None, None, None, [1700], None,
             "div(-7,2) = -3 (toward zero)"),
            ("SELECT div(5::numeric, 2::numeric);",
             [["2"]], None, None, None, [1700], None,
             "div(5,2) = 2 (truncates, never rounds up)"),
            # --- round() widening is a no-op, not 22003 ---
            ("SELECT round(3.14::numeric, 40);",
             [["3.14" + "0" * 38]], None, None, None, [1700], None,
             "round(3.14, 40) pads to dscale 40 (v0.61: PG display scale)"),
            # --- CTE VALUES bodies ---
            ("WITH v(x) AS (VALUES (1), (2), (3)) SELECT sum(x) FROM v;",
             [["6"]], None, None, None, [20], None,
             "WITH v(x) AS (VALUES ...) parses (was 42601); v0.76: sum(int4)->int8 like PG19"),
            ("WITH v(x) AS (VALUES ('0'::numeric), ('1'), ('-1')) SELECT x, x + 1 FROM v ORDER BY x;",
             [["-1", "0"], ["0", "1"], ["1", "2"]], None, None, None, [1700, 1700], None,
             "VALUES unknown literals coerce to the resolved numeric type"),
            # --- scale()/min_scale() on specials ---
            ("SELECT scale('NaN'::numeric);",
             [[None]], None, None, None, [23], None,
             "scale(NaN) is NULL"),
            ("SELECT scale('inf'::numeric);",
             [[None]], None, None, None, [23], None,
             "scale(inf) is NULL"),
            ("SELECT min_scale(1e100::numeric);",
             [["0"]], None, None, None, [23], None,
             "min_scale(1e100) clamps to 0"),
            # --- numeric(p, s) DDL with negative scale ---
            ("CREATE TABLE rg60_neg (x numeric(3,-6));",
             None, "CREATE TABLE", None, None, None, None,
             "numeric(3,-6) typmod parses (was 42601)"),
            ("DROP TABLE rg60_neg;",
             None, "DROP TABLE", None, None, None, None,
             "cleanup"),
            # --- PG19 underscore input rules ---
            ("SELECT '_123'::numeric;",
             None, None, "22P02", "invalid input syntax", None, None,
             "'_123' rejected (was accepted)"),
            ("SELECT '123._456'::numeric;",
             None, None, "22P02", "invalid input syntax", None, None,
             "'123._456' rejected (was accepted)"),
            ("SELECT '1.2e_34'::numeric;",
             None, None, "22P02", "invalid input syntax", None, None,
             "'1.2e_34' rejected (was accepted)"),
            ("SELECT '1_2'::numeric;",
             [["12"]], None, None, None, [1700], None,
             "'1_2' still accepted as 12"),
        ]

        for sql, want_rows, want_tag, want_code, want_msg, want_oids, want_cols, desc in cases:
            rows, tag, err, errcode, oids, cols = run_sql(s, sql)
            ok = True
            why = ""
            if want_rows is not None and rows != want_rows:
                ok = False
                why = f"rows {rows!r} != {want_rows!r}"
            if want_tag is not None and (tag is None or not tag.startswith(want_tag)):
                ok = False
                why = f"tag {tag!r} != {want_tag!r}"
            if want_code is not None and errcode != want_code:
                ok = False
                why = f"code {errcode!r} != {want_code!r} (err={err!r})"
            if want_msg is not None and (err is None or want_msg not in err):
                ok = False
                why = f"msg {err!r} missing {want_msg!r}"
            if want_oids is not None and oids != want_oids:
                ok = False
                why = f"oids {oids!r} != {want_oids!r}"
            if want_cols is not None and cols != want_cols:
                ok = False
                why = f"cols {cols!r} != {want_cols!r}"
            if errcode is not None and want_code is None:
                ok = False
                why = f"unexpected error {errcode}: {err}"
            if ok:
                passed += 1
            else:
                failed += 1
                print(f"FAIL: {desc}\n  sql: {sql}\n  {why}")

        print(f"protocol 60: {passed} passed, {failed} failed")
        return 1 if failed else 0
    finally:
        proc.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
