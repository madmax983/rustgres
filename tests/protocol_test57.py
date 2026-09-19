#!/usr/bin/env python3
"""Protocol 57: PG19-exact width_bucket + two-argument trunc.

v0.56 reimplements `width_bucket(op, b1, b2, count)` with exact decimal
arithmetic (new storage::BigUint/BigDec), matching PostgreSQL 19's
numeric and float8 variants, and adds PG's two-argument
`trunc(numeric, int)`:

* count <= 0 -> 22023 "count must be greater than zero".
* NaN operand -> count + 1 (either bound order); NaN *bounds* ->
  22003 "lower and upper bounds cannot be NaN".
* infinite bounds -> 22003 "lower and upper bounds must be finite";
  infinite operands are allowed (+inf -> count+1 / 0, -inf -> 0 /
  count+1 for ascending / descending bounds).
* equal bounds -> 22023 "lower bound cannot equal upper bound".
* descending bounds supported with PG's bucket numbering (above b1 ->
  0, below b2 -> count+1).
* huge numerics are exact: width_bucket(0, -1e100::numeric, 1, 10) = 10
  (float64 would round 1e100/(1e100+1) to 1.0 and yield 11).
* float8 overflow/underflow rows from PG's numeric.out regression test.
* bucket results beyond int32 -> 22003 "integer out of range".
* strict NULL propagation on all four arguments (vals[2]/vals[3] were
  not NULL-checked before).
* result type stays int4 (OID 23), column name "width_bucket".
* trunc(x, s): PG's trunc(numeric, int) -> numeric (OID 1700); float
  input goes through numeric like round's two-argument form.

RED on v0.55 (base 2e56ff5): equal bounds returned count+1, infinite
bounds were silently accepted, the descending 1e100 roundoff cases
returned 1, NULL in vals[2] raised 42883, and trunc(x, s) raised 42883.
(The ascending 1e100 cases happened to agree on v0.55 via result
clamping; they are regression guards for the exact-decimal rewrite.)
GREEN on v0.56: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5557
DATA_DIR = "/tmp/rg57proto"
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
    log = open("/tmp/rg57proto.log", "w")
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
        # OIDs: 23=int4, 701=float8, 1700=numeric
        cases = [
            # --- count validation (PG19 numeric.out) ---
            ("SELECT width_bucket(5.0, 3.0, 4.0, 0)",
             None, None, "22023", "count must be greater than zero", None, None,
             "count=0: 22023 (v0.55: 2201F)"),
            ("SELECT width_bucket(5.0, 3.0, 4.0, -5)",
             None, None, "22023", "count must be greater than zero", None, None,
             "count<0: 22023"),
            ("SELECT width_bucket(5.0::float8, 3.0::float8, 4.0::float8, -5)",
             None, None, "22023", "count must be greater than zero", None, None,
             "float8 count<0: 22023"),
            # --- equal bounds (v0.55 silently returned 889) ---
            ("SELECT width_bucket(3.5, 3.0, 3.0, 888)",
             None, None, "22023", "lower bound cannot equal upper bound", None, None,
             "equal numeric bounds: 22023 (v0.55: 889)"),
            ("SELECT width_bucket(3.5::float8, 3.0::float8, 3.0::float8, 888)",
             None, None, "22023", "lower bound cannot equal upper bound", None, None,
             "equal float8 bounds: 22023"),
            # --- NaN operand vs NaN bounds ---
            ("SELECT width_bucket('NaN', 3.0, 4.0, 888)",
             [["889"]], None, None, None, [23], ["width_bucket"],
             "NaN numeric operand -> count+1"),
            ("SELECT width_bucket('NaN'::float8, 3.0::float8, 4.0::float8, 888)",
             [["889"]], None, None, None, [23], ["width_bucket"],
             "NaN float8 operand -> count+1"),
            ("SELECT width_bucket(0, 'NaN', 4.0, 888)",
             None, None, "22003", "lower and upper bounds cannot be NaN", None, None,
             "NaN numeric bound: 22003 (v0.55: 889)"),
            ("SELECT width_bucket(0::float8, 'NaN', 4.0::float8, 888)",
             None, None, "22003", "lower and upper bounds cannot be NaN", None, None,
             "NaN float8 bound: 22003"),
            # --- infinite bounds rejected, infinite operands allowed ---
            ("SELECT width_bucket(2.0, 3.0, '-inf', 888)",
             None, None, "22003", "lower and upper bounds must be finite", None, None,
             "infinite numeric bound: 22003 (v0.55: 1)"),
            ("SELECT width_bucket(0.0::float8, 'Infinity'::float8, 5, 10)",
             None, None, "22003", "lower and upper bounds must be finite", None, None,
             "infinite float8 bound: 22003 (v0.55: 11)"),
            ("SELECT width_bucket('Infinity'::numeric, 1, 10, 10)",
             [["11"]], None, None, None, [23], ["width_bucket"],
             "+inf numeric operand ascending -> count+1"),
            ("SELECT width_bucket('-Infinity'::numeric, 1, 10, 10)",
             [["0"]], None, None, None, [23], ["width_bucket"],
             "-inf numeric operand ascending -> 0"),
            ("SELECT width_bucket('Infinity'::float8, 1, 10, 10)",
             [["11"]], None, None, None, [23], ["width_bucket"],
             "+inf float8 operand ascending -> count+1"),
            ("SELECT width_bucket('-Infinity'::float8, 1, 10, 10)",
             [["0"]], None, None, None, [23], ["width_bucket"],
             "-inf float8 operand ascending -> 0"),
            ("SELECT width_bucket('Infinity'::float8, 10, 1, 10)",
             [["0"]], None, None, None, [23], ["width_bucket"],
             "+inf float8 operand descending -> 0"),
            ("SELECT width_bucket('-Infinity'::float8, 10, 1, 10)",
             [["11"]], None, None, None, [23], ["width_bucket"],
             "-inf float8 operand descending -> count+1"),
            # --- normal operation, ascending ---
            ("SELECT width_bucket(-5.2, 0, 10, 5)",
             [["0"]], None, None, None, [23], ["width_bucket"],
             "below range -> 0"),
            ("SELECT width_bucket(10.0000000000001, 0, 10, 5)",
             [["6"]], None, None, None, [23], ["width_bucket"],
             "above range -> count+1"),
            ("SELECT width_bucket(4.5, 2, 8, 4)",
             [["2"]], None, None, None, [23], ["width_bucket"],
             "mid-range ascending"),
            ("SELECT width_bucket(5, 5.0, 5.5, 20)",
             [["1"]], None, None, None, [23], ["width_bucket"],
             "on lower bound -> 1"),
            ("SELECT width_bucket(5.5, 5.0, 5.5, 20)",
             [["21"]], None, None, None, [23], ["width_bucket"],
             "on upper bound -> count+1"),
            # --- descending bounds (PG bucket numbering) ---
            ("SELECT width_bucket(-5.2, 10, 0, 5)",
             [["6"]], None, None, None, [23], ["width_bucket"],
             "descending below b2 -> count+1"),
            ("SELECT width_bucket(10.0000000000001, 10, 0, 5)",
             [["0"]], None, None, None, [23], ["width_bucket"],
             "descending above b1 -> 0"),
            ("SELECT width_bucket(1, 10, 0, 5)",
             [["5"]], None, None, None, [23], ["width_bucket"],
             "descending mid-range"),
            ("SELECT width_bucket(2.00000000000001, 10, 0, 5)",
             [["4"]], None, None, None, [23], ["width_bucket"],
             "descending near edge"),
            # --- exactness: huge numerics (v0.55 returned 1) ---
            ("SELECT width_bucket(0, -1e100::numeric, 1, 10)",
             [["10"]], None, None, None, [23], ["width_bucket"],
             "1e100 numeric roundoff hazard -> 10 (v0.55 agreed via clamping)"),
            ("SELECT width_bucket(0, -1e100::float8, 1, 10)",
             [["10"]], None, None, None, [23], ["width_bucket"],
             "1e100 float8 roundoff hazard -> 10 (v0.55 agreed via clamping)"),
            ("SELECT width_bucket(1, 1e100::numeric, 0, 10)",
             [["10"]], None, None, None, [23], ["width_bucket"],
             "descending 1e100 numeric -> 10 (v0.55: 1)"),
            ("SELECT width_bucket(1, 1e100::float8, 0, 10)",
             [["10"]], None, None, None, [23], ["width_bucket"],
             "descending 1e100 float8 -> 10"),
            # --- float8 overflow/underflow rows (numeric.out LATERAL) ---
            ("SELECT width_bucket(10.5::float8, -1.797e308::float8, 1.797e308::float8, 2)",
             [["2"]], None, None, None, [23], ["width_bucket"],
             "huge float8 range, count=2"),
            ("SELECT width_bucket(10.5::float8, -1.797e308::float8, 1.797e308::float8, 3)",
             [["2"]], None, None, None, [23], ["width_bucket"],
             "huge float8 range, count=3"),
            ("SELECT width_bucket(4.4925e307::float8, -8.985e307::float8, 8.985e307::float8, 10)",
             [["8"]], None, None, None, [23], ["width_bucket"],
             "big/4 in symmetric huge range"),
            ("SELECT width_bucket(10.5::float8, 1.797e308::float8, -1.797e308::float8, 3)",
             [["2"]], None, None, None, [23], ["width_bucket"],
             "descending huge float8 range"),
            ("SELECT width_bucket(4.4925e307::float8, 8.985e307::float8, -8.985e307::float8, 10)",
             [["3"]], None, None, None, [23], ["width_bucket"],
             "descending big/4"),
            # NOTE: 5e-324::float8 underflows to 0 at literal-parse time
            # (pre-existing engine limitation); the numeric spelling below
            # exercises the same exact-decimal code path.
            ("SELECT width_bucket(0, 0, 5e-324, 4)",
             [["1"]], None, None, None, [23], ["width_bucket"],
             "tiny upper bound, op on lower"),
            ("SELECT width_bucket(5e-324, 0, 5e-324, 4)",
             [["5"]], None, None, None, [23], ["width_bucket"],
             "tiny upper bound, op on upper"),
            ("SELECT width_bucket(0, 0, 1, 2147483647)",
             [["1"]], None, None, None, [23], ["width_bucket"],
             "max count, op on lower bound"),
            ("SELECT width_bucket(1, 1, 0, 2147483647)",
             [["1"]], None, None, None, [23], ["width_bucket"],
             "max count descending, op on bound"),
            # --- int32 overflow of the result ---
            ("SELECT width_bucket(1::float8, 0, 1, 2147483647)",
             None, None, "22003", "integer out of range", None, None,
             "bucket 2^31: 22003 (v0.55 wrapped)"),
            ("SELECT width_bucket(0::float8, 1, 0, 2147483647)",
             None, None, "22003", "integer out of range", None, None,
             "descending bucket 2^31: 22003"),
            # --- strict NULL propagation ---
            ("SELECT width_bucket(NULL, 0, 10, 5)",
             [[None]], None, None, None, [23], ["width_bucket"],
             "NULL operand -> NULL"),
            ("SELECT width_bucket(1, NULL, 10, 5)",
             [[None]], None, None, None, [23], ["width_bucket"],
             "NULL b1 -> NULL"),
            ("SELECT width_bucket(1, 0, NULL, 5)",
             [[None]], None, None, None, [23], ["width_bucket"],
             "NULL b2 -> NULL (v0.55: 42883)"),
            ("SELECT width_bucket(1, 0, 10, NULL)",
             [[None]], None, None, None, [23], ["width_bucket"],
             "NULL count -> NULL"),
            # --- two-argument trunc (PG's trunc(numeric, int)) ---
            ("SELECT trunc(19.99, 1)",
             [["19.9"]], None, None, None, [1700], ["trunc"],
             "trunc(numeric, 1) (v0.55: 42883)"),
            ("SELECT trunc(19.99, -1)",
             [["10"]], None, None, None, [1700], ["trunc"],
             "trunc(numeric, -1)"),
            ("SELECT trunc(1.99999, 3)",
             [["1.999"]], None, None, None, [1700], ["trunc"],
             "trunc(numeric, 3)"),
            ("SELECT trunc(-19.99, 1)",
             [["-19.9"]], None, None, None, [1700], ["trunc"],
             "trunc negative"),
            ("SELECT trunc(19.99::float8, 1)",
             [["19.9"]], None, None, None, [1700], ["trunc"],
             "trunc(float8, int) -> numeric (v0.55: 42883)"),
            ("SELECT trunc(123.456, 0)",
             [["123"]], None, None, None, [1700], ["trunc"],
             "trunc(numeric, 0)"),
            ("SELECT trunc(1.5, -2147483648)",
             [["0"]], None, None, None, [1700], ["trunc"],
             "huge negative scale -> 0, no overflow panic"),
            ("SELECT trunc(1.5, NULL)",
             [[None]], None, None, None, [1700], ["trunc"],
             "NULL scale -> NULL"),
            ("SELECT trunc(1.5, 1, 2)",
             None, None, "42883", None, None, None,
             "trunc/3 still 42883"),
            # --- one-argument trunc unchanged ---
            ("SELECT trunc(19.99)",
             [["19"]], None, None, None, [1700], ["trunc"],
             "trunc(numeric) unchanged"),
            ("SELECT trunc(9.9::float8)",
             [["9"]], None, None, None, [701], ["trunc"],
             "trunc(float8) still float8"),
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
            else:
                failed += 1
                print(f"FAIL: {desc}\n  sql: {sql}\n  {why}")
        print(f"protocol 57: {passed} passed, {failed} failed")
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
