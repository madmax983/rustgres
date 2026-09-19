#!/usr/bin/env python3
"""Protocol 61: PG19 numeric(p,s) typmod enforcement on assignment.

v0.60 stores the numeric typmod (PostgreSQL 19 numerictypmodin:
precision 1..1000, scale -1000..1000, 22023 otherwise) and applies
PG19's apply_typmod on every assignment path (INSERT/UPDATE literals,
casts, evaluated values):

* the value is rounded to the declared scale first (ties away from
  zero, like PG), THEN the precision check runs on the rounded value
* +/-Infinity is 22003 in a typmod-constrained column; NaN passes
  through unchanged
* negative scales round left of the decimal point and keep a 0
  display scale (PG19's own regression output shows scale() = 0 for
  numeric(3,-6) values)

Grounded in the bundled PG19 numeric.out `fract_only` (numeric(4,4))
and `num_typemod_test` (numeric(3,-6/-3/0/3/6)) cases.

RED on v0.59 (base 4b2d79f3): typmod syntax parsed but discarded, so
0.99994 stays 0.99994, and 1.0 / 0.99995 / 999500000 insert without
error.
GREEN on v0.60: rounding + 22003 overflow + Infinity rejection.
"""
import socket, struct, subprocess, time, os

PORT = 5561
DATA_DIR = "/tmp/rg61proto"
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
    log = open("/tmp/rg61proto.log", "w")
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
        # OIDs: 1700=numeric
        cases = [
            # --- numeric(4,4): round to scale, then overflow (PG19 fract_only) ---
            ("CREATE TABLE rg61_fract (x numeric(4,4));",
             None, "CREATE TABLE", None, None, None, None,
             "numeric(4,4) DDL keeps the typmod"),
            ("INSERT INTO rg61_fract VALUES (0.99994), (0.00017);",
             None, "INSERT", None, None, None, None,
             "inserts that need rounding succeed"),
            ("SELECT x FROM rg61_fract ORDER BY x;",
             [["0.0002"], ["0.9999"]], None, None, None, [1700], None,
             "0.99994 -> 0.9999, 0.00017 -> 0.0002 (was stored unrounded)"),
            ("INSERT INTO rg61_fract VALUES (1.0);",
             None, None, "22003", "numeric field overflow", None, None,
             "1.0 rejected (was accepted)"),
            ("INSERT INTO rg61_fract VALUES (0.99995);",
             None, None, "22003", "numeric field overflow", None, None,
             "0.99995 rounds to 1.0000 -> overflow (was accepted)"),
            ("INSERT INTO rg61_fract VALUES ('Infinity');",
             None, None, "22003", "numeric field overflow", None, None,
             "Infinity rejected in typmod column (was accepted)"),
            ("INSERT INTO rg61_fract VALUES ('NaN');",
             None, "INSERT", None, None, None, None,
             "NaN passes through unchanged"),
            ("UPDATE rg61_fract SET x = 0.00016;",
             None, "UPDATE", None, None, None, None,
             "UPDATE applies the typmod too"),
            ("SELECT count(*) FROM rg61_fract WHERE x = 0.0002;",
             [["3"]], None, None, None, [20], None,
             "updated rows round to 0.0002"),
            # --- numeric(3,-6): negative scale rounds left of the point ---
            ("CREATE TABLE rg61_neg (x numeric(3,-6));",
             None, "CREATE TABLE", None, None, None, None,
             "numeric(3,-6) DDL"),
            ("INSERT INTO rg61_neg VALUES (123456), (654321);",
             None, "INSERT", None, None, None, None,
             "inserts succeed"),
            ("SELECT x FROM rg61_neg ORDER BY x;",
             [["0"], ["1000000"]], None, None, None, [1700], None,
             "123456 -> 0, 654321 -> 1000000 (was stored unrounded)"),
            ("INSERT INTO rg61_neg VALUES (999500000);",
             None, None, "22003", "numeric field overflow", None, None,
             "999500000 rounds to 10^9 -> overflow (was accepted)"),
            # --- casts apply the typmod ---
            ("SELECT 0.99994::numeric(4,4);",
             [["0.9999"]], None, None, None, [1700], None,
             "cast rounds to scale"),
            ("SELECT 2.0::numeric(4,4);",
             None, None, "22003", "numeric field overflow", None, None,
             "cast overflow is 22003"),
            ("SELECT CAST('12.345' AS numeric(5,2));",
             [["12.35"]], None, None, None, [1700], None,
             "CAST('12.345' AS numeric(5,2)) = 12.35"),
            # --- invalid typmods are 22023 ---
            ("CREATE TABLE rg61_bad (x numeric(0,1));",
             None, None, "22023", "precision", None, None,
             "precision 0 rejected (was accepted)"),
            ("DROP TABLE rg61_fract;",
             None, "DROP TABLE", None, None, None, None,
             "cleanup"),
            ("DROP TABLE rg61_neg;",
             None, "DROP TABLE", None, None, None, None,
             "cleanup"),
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
            if ok:
                passed += 1
                print(f"PASS: {desc}")
            else:
                failed += 1
                print(f"FAIL: {desc}: {why}")

        print(f"\n{passed} passed, {failed} failed")
        return 1 if failed else 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    raise SystemExit(main())
