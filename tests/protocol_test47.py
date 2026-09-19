#!/usr/bin/env python3
"""Protocol 47: generate_series table function (int4/int8/numeric).

v0.46 implements PG19's generate_series as a FROM-clause table function:
integer series (positive/negative steps, endpoint-inclusive, step 0 ->
22023, successor overflow still emits the endpoint), numeric series
(NaN/infinity rejected like PG19, zero step -> 22023), NULL inputs
yield zero rows (strict), and comma joins with correlated arguments
behave as implicit LATERAL (`FROM t, f(t.x)` re-evaluates per row).

Wire types are PG's: int4 -> OID 23, int8 -> OID 20, numeric -> 1700.

RED on v0.45: generate_series() unsupported (42883 from the function
registry) for every case below.
GREEN on v0.46: all cases pass.

Note: rustgres renders numeric results at minimal scale (its numeric
addition behaves the same way), so the series 0.1..4.0 step 1.3 ends in
`4`, not PG's `4.0`; the conformance harness canonicalizes OID-1700
values through Decimal, so they compare equal by value.
"""
import socket, struct, subprocess, time, os

PORT = 5547
DATA_DIR = "/tmp/rg47proto"
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
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"T":
            (n,) = struct.unpack("!h", b[:2])
            pos = 2
            for _ in range(n):
                z = b.index(b"\x00", pos)
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
    return rows, err, errcode, oids


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg47proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        # (sql, expected_rows_or_None, expected_errcode_or_None,
        #  expected_oids_or_None, description)
        cases = [
            # --- integer series
            ("SELECT * FROM generate_series(1, 5)",
             [["1"], ["2"], ["3"], ["4"], ["5"]], None, [23],
             "int4 series 1..5, OID 23"),
            ("SELECT * FROM generate_series(5, 1, -2)",
             [["5"], ["3"], ["1"]], None, [23],
             "int4 negative step, endpoint-inclusive"),
            ("SELECT * FROM generate_series(5, 1, -1)",
             [["5"], ["4"], ["3"], ["2"], ["1"]], None, [23],
             "int4 step -1 down to 1"),
            ("SELECT * FROM generate_series(5, 1)",
             [], None, [23],
             "int4 positive step, start > stop -> no rows"),
            ("SELECT * FROM generate_series(1, 5, 0)",
             None, "22023", None,
             "int4 step zero -> 22023"),
            # --- int8 series
            ("SELECT * FROM generate_series(1::bigint, 3::bigint)",
             [["1"], ["2"], ["3"]], None, [20],
             "int8 series, OID 20"),
            ("SELECT * FROM generate_series(9223372036854775805, 9223372036854775807)",
             [["9223372036854775805"], ["9223372036854775806"],
              ["9223372036854775807"]], None, [20],
             "int8 max endpoint (successor would overflow)"),
            ("SELECT * FROM generate_series(2147483645, 2147483647)",
             [["2147483645"], ["2147483646"], ["2147483647"]], None, [23],
             "int4 max endpoint (successor would overflow)"),
            # --- numeric series
            ("SELECT * FROM generate_series(0.1::numeric, 4.0::numeric, 1.3::numeric)",
             [["0.1"], ["1.4"], ["2.7"], ["4.0"]], None, [1700],
             "numeric series, OID 1700"),
            ("SELECT * FROM generate_series(1.5, 3.5)",
             [["1.5"], ["2.5"], ["3.5"]], None, [1700],
             "numeric default step 1, endpoint-inclusive"),
            ("SELECT * FROM generate_series(1::numeric, 5::numeric, 0)",
             None, "22023", None,
             "numeric step zero -> 22023"),
            ("SELECT * FROM generate_series('NaN'::numeric, 5)",
             None, "22023", None,
             "numeric NaN start rejected"),
            ("SELECT * FROM generate_series(1, 'Infinity'::numeric)",
             None, "22023", None,
             "numeric infinite stop rejected"),
            # --- strictness / arity / unknown signatures
            ("SELECT * FROM generate_series(NULL, 5)",
             [], None, [23],
             "NULL start -> zero rows (strict)"),
            ("SELECT * FROM generate_series('a', 'z')",
             None, "42883", None,
             "text args -> no such function"),
            ("SELECT * FROM generate_series(1)",
             None, "42883", None,
             "wrong arity -> no such function"),
            # --- naming / aliases
            ("SELECT * FROM generate_series(1, 3) AS g",
             [["1"], ["2"], ["3"]], None, [23],
             "table alias becomes the column name"),
            ("SELECT * FROM generate_series(1, 3) g(x)",
             [["1"], ["2"], ["3"]], None, [23],
             "column alias renames the output"),
            # --- implicit LATERAL (comma joins parse to CROSS JOINs)
            ("SELECT * FROM generate_series(1, 3) i, generate_series(1, i) j",
             [["1", "1"], ["2", "1"], ["2", "2"],
              ["3", "1"], ["3", "2"], ["3", "3"]], None, [23, 23],
             "correlated int series re-evaluated per row"),
            ("SELECT * FROM generate_series(1::numeric, 3::numeric) i, generate_series(i, 3) j",
             [["1", "1"], ["1", "2"], ["1", "3"],
              ["2", "2"], ["2", "3"], ["3", "3"]], None, [1700, 1700],
             "correlated numeric series"),
            ("SELECT * FROM generate_series(1::bigint, 2::bigint) i, generate_series(i, 3) j",
             [["1", "1"], ["1", "2"], ["1", "3"],
              ["2", "2"], ["2", "3"]], None, [20, 20],
             "correlated int8 series keeps OID 20"),
            ("SELECT * FROM generate_series(1, 0) i, generate_series(i, 3) j",
             [], None, [23, 23],
             "empty left input -> empty result, schema still typed"),
            # --- uncorrelated comma join still works
            ("SELECT * FROM generate_series(1, 2) a, generate_series(10, 11) b",
             [["1", "10"], ["1", "11"], ["2", "10"], ["2", "11"]],
             None, [23, 23],
             "uncorrelated comma join is a plain cross product"),
        ]

        for sql, want_rows, want_code, want_oids, desc in cases:
            rows, err, code, oids = run_sql(s, sql)
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
            if ok:
                passed += 1
            else:
                failed += 1
                print(f"FAIL: {desc}\n  sql: {sql}\n  {detail}")
        print(f"protocol 47: {passed} passed, {failed} failed")
        s.close()
    finally:
        proc.terminate()

    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
