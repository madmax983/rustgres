#!/usr/bin/env python3
"""Protocol 59: binary float I/O — float4send/float4recv/float8recv (PG19).

v0.58 adds PostgreSQL 19's binary float send/receive functions:

* float4send(float4) -> bytea: 4-byte big-endian IEEE 754 (PG19 float.c:
  pq_sendfloat4 = htonl of the bit pattern);
* float4recv(bytea) -> float4 / float8recv(bytea) -> float8: big-endian
  IEEE 754 decode (pq_getmsgfloat4/8); fewer than 4/8 bytes is 22P03
  "insufficient data left in message"; trailing bytes ignored, as in PG.

Correctness anchor: PG19 float4in uses strtof (single correct rounding),
and rustgres parses float4 via Rust's correctly-rounded f32 parse, so
the edge-case inputs from PG's own float4 regression tests
("Test for correct input rounding in edge cases", Paxson 1991) produce
bit-identical bytes — e.g. '7038531e-32' -> 0x15ae43fd, where naive
double rounding (f64 then narrow) would give 0x15ae43fe.

RED on v0.57 (base f6f08144): functions do not exist (42883).
GREEN on v0.58: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5559
DATA_DIR = "/tmp/rg59proto"
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
    log = open("/tmp/rg59proto.log", "w")
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
        # OIDs: 17=bytea, 700=float4, 701=float8
        cases = [
            # --- float4send: PG regression edge cases, bit-exact ---
            ("SELECT float4send('5e-20'::float4);",
             [[r"\x1f6c1e4a"]], None, None, None, [17], ["float4send"],
             "float4send('5e-20') = \\x1f6c1e4a (PG expected)"),
            ("SELECT float4send('7038531e-32'::float4);",
             [[r"\x15ae43fd"]], None, None, None, [17], ["float4send"],
             "correct single rounding: 15ae43fd, not double-rounded 15ae43fe"),
            ("SELECT float4send('1.17549435e-38'::float4);",
             [[r"\x00800000"]], None, None, None, [17], ["float4send"],
             "smallest normalized float4 input"),
            ("SELECT float4send('0'::float4);",
             [[r"\x00000000"]], None, None, None, [17], None,
             "zero"),
            ("SELECT float4send('nan'::float4);",
             [[r"\x7fc00000"]], None, None, None, [17], None,
             "NaN canonical bits"),
            ("SELECT float4send('inf'::float4);",
             [[r"\x7f800000"]], None, None, None, [17], None,
             "+Infinity"),
            ("SELECT float4send('-inf'::float4);",
             [[r"\xff800000"]], None, None, None, [17], None,
             "-Infinity"),
            ("SELECT float4send(NULL::float4);",
             [[None]], None, None, None, [17], None,
             "strict: NULL in -> NULL out"),
            # --- float4recv / float8recv ---
            ("SELECT float4recv('\\x3f800000'::bytea);",
             [["1"]], None, None, None, [700], ["float4recv"],
             "float4recv decodes 1.0; OID 700 on the wire"),
            ("SELECT float4recv(float4send('3.14'::float4)) = '3.14'::float4;",
             [["t"]], None, None, None, None, None,
             "send/recv round-trip is identity"),
            ("SELECT float8recv(float8send('2.5'::float8)) = '2.5'::float8;",
             [["t"]], None, None, None, [16], None,
             "float8 send/recv round-trip; result is bool"),
            ("SELECT float4recv('\\x0102'::bytea);",
             None, None, "22P03", "insufficient data left in message", None, None,
             "short bytea -> 22P03 (PG's pq_getmsgfloat4)"),
            ("SELECT float8recv('\\x01020304050607'::bytea);",
             None, None, "22P03", "insufficient data left in message", None, None,
             "7-byte input to float8recv -> 22P03"),
            # --- arity / type errors ---
            ("SELECT float4send('1'::float4, '2'::float4);",
             None, None, "42883", None, None, None,
             "wrong arity -> 42883"),
            ("SELECT float4recv(123);",
             None, None, "42883", None, None, None,
             "wrong arg type -> 42883"),
        ]

        for sql, want_rows, want_tag, want_code, want_msg, want_oids, want_cols, desc in cases:
            rows, tag, err, errcode, oids, cols = run_sql(s, sql)
            ok = True
            why = ""
            if want_rows is not None and rows != want_rows:
                ok = False
                why = f"rows {rows!r} != {want_rows!r}"
            if want_tag is not None and tag != want_tag:
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

        print(f"protocol 59: {passed} passed, {failed} failed")
        return 1 if failed else 0
    finally:
        proc.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
