#!/usr/bin/env python3
"""Protocol 51: integer overflow detection (SQLSTATE 22003) for + - * / %.

v0.50 grounds integer arithmetic in PG19's int.c/int8.c (REL_19_STABLE):

* int2/int2 stays int2: pg_add/sub/mul_s16_overflow -> 22003
  "smallint out of range". NO promotion to integer (the v0.49 code
  wrongly promoted smallint pairs to int4, so
  (-32768)::int2 * (-1)::int2 returned 32768 instead of erroring).
* int4/int4 and mixed int2/int4 stay int4: pg_add/sub/mul_s32_overflow ->
  22003 "integer out of range".
* int8/int8 and mixed with int8 stay int8: pg_add/sub/mul_s64_overflow ->
  22003 "bigint out of range".
* division: divisor 0 -> 22012; INT_MIN / -1 -> 22003 (same per-type
  message); otherwise exact.
* modulo: divisor 0 -> 22012; INT_MIN % -1 is 0 (never an error).
* non-overflowing results keep the PG result type (int2 -> OID 21,
  int4 -> OID 23, int8 -> OID 20).

Note on the message: PG19's arithmetic overflow message is the bare
"smallint out of range" (int2.out etc.); the
'value "32768" is out of range for type smallint' wording is only for
literal *input*. The test asserts PG's actual arithmetic messages.

RED on v0.49: the int2 overflow cases return success (wrong promotion);
the int8 message cases report "integer out of range" instead of
"bigint out of range".
GREEN on v0.50: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5551
DATA_DIR = "/tmp/rg51proto"
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
    log = open("/tmp/rg51proto.log", "w")
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
        cases = [
            # --- int2: overflow -> 22003 "smallint out of range", no promotion
            ("SELECT (-32768)::int2 * (-1)::int2",
             None, None, "22003", "smallint out of range", None, None,
             "int2 mul INT16_MIN * -1 -> 22003"),
            ("SELECT (-32768)::int2 / (-1)::int2",
             None, None, "22003", "smallint out of range", None, None,
             "int2 div INT16_MIN / -1 -> 22003"),
            ("SELECT (-32768)::int2 % (-1)::int2",
             [["0"]], None, None, None, [21], None,
             "int2 mod INT16_MIN % -1 is 0, no error"),
            ("SELECT 32767::int2 + 1::int2",
             None, None, "22003", "smallint out of range", None, None,
             "int2 add overflow -> 22003"),
            ("SELECT (-32768)::int2 - 1::int2",
             None, None, "22003", "smallint out of range", None, None,
             "int2 sub underflow -> 22003"),
            ("SELECT 30000::int2 + 30000::int2",
             None, None, "22003", "smallint out of range", None, None,
             "int2+int2 does NOT promote to int4 (PG errors)"),
            ("SELECT 200::int2 * 200::int2",
             None, None, "22003", "smallint out of range", None, None,
             "int2 mul overflow -> 22003"),
            ("SELECT 1::int2 + 2::int2",
             [["3"]], None, None, None, [21], None,
             "int2+int2 stays smallint (OID 21)"),
            ("SELECT 32767::int2 / 1::int2",
             [["32767"]], None, None, None, [21], None,
             "int2 div stays smallint (OID 21)"),
            ("SELECT (-7)::int2 % 3::int2",
             [["-1"]], None, None, None, [21], None,
             "int2 mod keeps dividend sign"),
            ("SELECT 1::int2 / 0::int2",
             None, None, "22012", "division by zero", None, None,
             "int2 div by zero still 22012"),
            # --- int4: overflow -> 22003 "integer out of range"
            ("SELECT (-2147483648)::int4 * (-1)::int4",
             None, None, "22003", "integer out of range", None, None,
             "int4 mul INT32_MIN * -1 -> 22003"),
            ("SELECT (-2147483648)::int4 / (-1)::int4",
             None, None, "22003", "integer out of range", None, None,
             "int4 div INT32_MIN / -1 -> 22003"),
            ("SELECT (-2147483648)::int4 % (-1)::int4",
             [["0"]], None, None, None, [23], None,
             "int4 mod INT32_MIN % -1 is 0, no error"),
            ("SELECT 2147483647::int4 + 1::int4",
             None, None, "22003", "integer out of range", None, None,
             "int4 add overflow -> 22003"),
            ("SELECT (-2147483648)::int4 - 1::int4",
             None, None, "22003", "integer out of range", None, None,
             "int4 sub underflow -> 22003"),
            ("SELECT 100000::int4 * 100000::int4",
             None, None, "22003", "integer out of range", None, None,
             "int4 mul overflow -> 22003"),
            ("SELECT (-2147483648)::int4 * (-1)::int2",
             None, None, "22003", "integer out of range", None, None,
             "int4*int2 mixed -> 22003 integer out of range"),
            ("SELECT (-2147483648)::int4 / (-1)::int2",
             None, None, "22003", "integer out of range", None, None,
             "int4/int2 mixed -> 22003 integer out of range"),
            ("SELECT (-2147483648)::int4 % (-1)::int2",
             [["0"]], None, None, None, [23], None,
             "int4%int2 mixed INT32_MIN % -1 is 0"),
            ("SELECT 1::int4 + 2::int4",
             [["3"]], None, None, None, [23], None,
             "int4+int4 stays integer (OID 23)"),
            ("SELECT 1::int4 / 0::int4",
             None, None, "22012", "division by zero", None, None,
             "int4 div by zero still 22012"),
            # --- int8: overflow -> 22003 "bigint out of range"
            ("SELECT (-9223372036854775808)::int8 * (-1)::int8",
             None, None, "22003", "bigint out of range", None, None,
             "int8 mul INT64_MIN * -1 -> 22003"),
            ("SELECT (-9223372036854775808)::int8 / (-1)::int8",
             None, None, "22003", "bigint out of range", None, None,
             "int8 div INT64_MIN / -1 -> 22003"),
            ("SELECT (-9223372036854775808)::int8 % (-1)::int8",
             [["0"]], None, None, None, [20], None,
             "int8 mod INT64_MIN % -1 is 0, no error"),
            ("SELECT 9223372036854775807::int8 + 1::int8",
             None, None, "22003", "bigint out of range", None, None,
             "int8 add overflow -> 22003"),
            ("SELECT (-9223372036854775808)::int8 - 1::int8",
             None, None, "22003", "bigint out of range", None, None,
             "int8 sub underflow -> 22003"),
            ("SELECT 3037000500::int8 * 3037000500::int8",
             None, None, "22003", "bigint out of range", None, None,
             "int8 mul overflow -> 22003"),
            ("SELECT (-9223372036854775808)::int8 * (-1)::int4",
             None, None, "22003", "bigint out of range", None, None,
             "int8*int4 mixed -> 22003 bigint out of range"),
            ("SELECT (-9223372036854775808)::int8 / (-1)::int4",
             None, None, "22003", "bigint out of range", None, None,
             "int8/int4 mixed -> 22003 bigint out of range"),
            ("SELECT (-9223372036854775808)::int8 % (-1)::int4",
             [["0"]], None, None, None, [20], None,
             "int8%int4 mixed INT64_MIN % -1 is 0"),
            ("SELECT (-9223372036854775808)::int8 * (-1)::int2",
             None, None, "22003", "bigint out of range", None, None,
             "int8*int2 mixed -> 22003 bigint out of range"),
            ("SELECT 1::int8 + 2::int8",
             [["3"]], None, None, None, [20], None,
             "int8+int8 stays bigint (OID 20)"),
            ("SELECT 1::int8 % 0::int8",
             None, None, "22012", "division by zero", None, None,
             "int8 mod by zero still 22012"),
            # --- boundary values still compute (no false positives)
            ("SELECT 32767::int2 + 0::int2",
             [["32767"]], None, None, None, [21], None,
             "int2 max boundary exact"),
            ("SELECT (-32768)::int2 * 1::int2",
             [["-32768"]], None, None, None, [21], None,
             "int2 min boundary exact"),
            ("SELECT 2147483647::int4 + 0::int4",
             [["2147483647"]], None, None, None, [23], None,
             "int4 max boundary exact"),
            ("SELECT 9223372036854775807::int8 - 0::int8",
             [["9223372036854775807"]], None, None, None, [20], None,
             "int8 max boundary exact"),
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
        print(f"protocol 51: {passed} passed, {failed} failed")
        s.close()
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
