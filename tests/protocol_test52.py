#!/usr/bin/env python3
"""Protocol 52: int2 bitwise operators return int2 (PG19-exact).

v0.51 grounds the int2 bitwise operators in PG19's int.c
(REL_19_STABLE) and pg_operator.dat (lines ~4398-4410):

* int2and / int2or / int2xor / int2shl / int2shr all PG_RETURN_INT16:
  int2 <op> int2 -> int2 (OID 21). The v0.50 code wrongly promoted
  int2 pairs to int4 (OID 23).
* Shifts use C integer promotion: the int16 operands are promoted to
  int, the shift happens in 32 bits, then the result is truncated to
  int16. It *wraps* — never raises 22003. E.g. 1::int2 << 15 is
  -32768 (the 1 bit lands in the sign position), NOT an overflow error.
* Unary `~` is deliberately unchanged: PG19 has NO int2 `~` operator
  (only int4), so ~int2 -> int4 (OID 23) is correct.
* int4 `& | # << >>` return int4 (OID 23); int8 `& |` return int8
  (OID 20) — already correct in v0.50, covered here as regression
  tests.
* NULL propagates (strict functions in PG).

RED on v0.50: every int2 case fails (rows come back as int4 values
with OID 23; 1::int2 << 15 returns 32768 instead of -32768).
GREEN on v0.51: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5552
DATA_DIR = "/tmp/rg52proto"
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
    log = open("/tmp/rg52proto.log", "w")
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
            # --- int2: result stays smallint, OID 21 ---
            ("SELECT 12::int2 & 10::int2",
             [["8"]], None, None, None, [21], None,
             "int2 & int2 -> int2 (OID 21)"),
            ("SELECT 12::int2 | 10::int2",
             [["14"]], None, None, None, [21], None,
             "int2 | int2 -> int2 (OID 21)"),
            ("SELECT 12::int2 # 10::int2",
             [["6"]], None, None, None, [21], None,
             "int2 # int2 -> int2 (OID 21)"),
            ("SELECT (-1)::int2 & 32767::int2",
             [["32767"]], None, None, None, [21], None,
             "int2 & sign-bit truncation stays int2"),
            # --- int2 shifts: C wrap-then-truncate, never 22003 ---
            ("SELECT 1::int2 << 15::int2",
             [["-32768"]], None, None, None, [21], None,
             "1::int2 << 15 wraps to -32768 (int.c int2shl)"),
            ("SELECT 20000::int2 << 2::int2",
             [["14464"]], None, None, None, [21], None,
             "20000::int2 << 2 = 0x13880 -> 0x3880 = 14464"),
            ("SELECT 3::int2 << 14::int2",
             [["-16384"]], None, None, None, [21], None,
             "3::int2 << 14 = 49152 -> -16384"),
            ("SELECT 1::int2 << 14::int2",
             [["16384"]], None, None, None, [21], None,
             "1::int2 << 14 = 16384 (no false wrap)"),
            ("SELECT (-1)::int2 >> 1::int2",
             [["-1"]], None, None, None, [21], None,
             "int2 >> is an arithmetic right shift"),
            ("SELECT (-32768)::int2 >> 15::int2",
             [["-1"]], None, None, None, [21], None,
             "int2 >> 15 sign-fills to -1"),
            # --- NULL propagates ---
            ("SELECT NULL::int2 & 1::int2",
             [[None]], None, None, None, [21], None,
             "NULL int2 bitwise -> NULL, still OID 21"),
            # --- int4: unchanged semantics (regression coverage) ---
            ("SELECT 12::int4 & 10::int4",
             [["8"]], None, None, None, [23], None,
             "int4 & int4 -> int4 (OID 23)"),
            ("SELECT 12::int4 | 10::int4",
             [["14"]], None, None, None, [23], None,
             "int4 | int4 -> int4 (OID 23)"),
            ("SELECT 12::int4 # 10::int4",
             [["6"]], None, None, None, [23], None,
             "int4 # int4 -> int4 (OID 23)"),
            ("SELECT 1::int4 << 31::int4",
             [["-2147483648"]], None, None, None, [23], None,
             "1::int4 << 31 wraps to INT32_MIN"),
            ("SELECT (-1)::int4 >> 1::int4",
             [["-1"]], None, None, None, [23], None,
             "int4 >> arithmetic shift"),
            # --- int8: unchanged semantics (regression coverage) ---
            ("SELECT 12::int8 & 10::int8",
             [["8"]], None, None, None, [20], None,
             "int8 & int8 -> int8 (OID 20)"),
            ("SELECT 12::int8 | 10::int8",
             [["14"]], None, None, None, [20], None,
             "int8 | int8 -> int8 (OID 20)"),
            ("SELECT 1::int8 << 63::int8",
             [["-9223372036854775808"]], None, None, None, [20], None,
             "1::int8 << 63 wraps to INT64_MIN"),
            # --- unary ~ still promotes smallint to int4 (PG has no int2not).
            # Note: rustgres parses `~5::int2` as `(~5)::int2` (`::` binds
            # looser than unary ops here, a pre-existing deviation from PG),
            # so the parens below pin the genuine PG semantic explicitly.
            ("SELECT ~(5::int2)",
             [["-6"]], None, None, None, [23], None,
             "~(int2) -> int4 (OID 23), unchanged"),
            ("SELECT ~(5::int8)",
             [["-6"]], None, None, None, [20], None,
             "~(int8) -> int8 (OID 20), unchanged"),
            # --- mixed widths resolve to the wider type ---
            ("SELECT 5::int2 | 2::int4",
             [["7"]], None, None, None, [23], None,
             "int2|int4 mixed -> int4 (OID 23)"),
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
        print(f"protocol 52: {passed} passed, {failed} failed")
        s.close()
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
