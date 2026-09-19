#!/usr/bin/env python3
"""Protocol 58: PG19 `name` type — 63-byte truncation (namein) + nameeq.

v0.57 gives PostgreSQL's internal identifier type `name` (OID 19) its own
identity instead of aliasing it to text:

* input truncated at NAMEDATALEN-1 = 63 bytes (PG19 namein), silently —
  on INSERT, on explicit `::name` casts, on typed literals `name '...'`,
  and on extended-protocol parameters bound to a name column;
* comparisons coerce the unknown-type literal side through namein too,
  so `f1 = '<64 chars>'` matches the truncated stored row (PG's
  strncmp(..., NAMEDATALEN) semantics — plain byte order on truncated
  values, since names hold no NULs);
* RowDescription reports OID 19 for name columns;
* set-operation type resolution: name+name -> name, name+text -> text.

RED on v0.56 (base 34e099f0): `name` was an alias for text — no
truncation (length 64), OID 25 on the wire, and the 64-char literal
comparison matched nothing.
GREEN on v0.57: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5558
DATA_DIR = "/tmp/rg58proto"
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


L64 = "1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890ABCDEFGHIJKLMNOPQR"
L63 = L64[:63]
assert len(L64) == 64 and len(L63) == 63


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg58proto.log", "w")
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
        # OIDs: 19=name, 23=int4, 25=text
        cases = [
            ("CREATE TABLE name_tbl(f1 name);",
             None, "CREATE TABLE", None, None, None, None,
             "CREATE TABLE with name column"),
            (f"INSERT INTO name_tbl(f1) VALUES ('{L64}'),('asdf'),('');",
             None, "INSERT 0 3", None, None, None, None,
             "INSERT 64-char / short / empty literals"),
            # --- namein truncation at 63 bytes ---
            ("SELECT f1, length(f1) FROM name_tbl ORDER BY f1;",
             [["", "0"], [L63, "63"], ["asdf", "4"]], None, None, None,
             [19, 23], ["f1", "length"],
             "64-char value truncated to 63 bytes; OID 19 on the wire"),
            (f"SELECT '{L64}'::name, length('{L64}'::name);",
             [[L63, "63"]], None, None, None, [19, 23], None,
             "explicit cast truncates to 63 bytes"),
            (f"SELECT name '{L64}', length(name '{L64}');",
             [[L63, "63"]], None, None, None, [19, 23], None,
             "typed literal name '...' truncates"),
            # --- nameeq: literal side coerced through namein ---
            (f"SELECT f1 FROM name_tbl WHERE f1 = '{L64}';",
             [[L63]], None, None, None, [19], ["f1"],
             "= with 64-char literal matches truncated row (v0.56: 0 rows)"),
            (f"SELECT f1 FROM name_tbl WHERE f1 <> '{L64}' ORDER BY f1;",
             [["", ], ["asdf"]], None, None, None, [19], ["f1"],
             "<> with 64-char literal returns the other rows"),
            (f"SELECT f1 FROM name_tbl WHERE f1 < '{L64}' ORDER BY f1;",
             [[""]], None, None, None, [19], ["f1"],
             "< with 64-char literal: only '' sorts below"),
            (f"SELECT count(*) FROM name_tbl WHERE f1 >= '{L64}';",
             [["2"]], None, None, None, [20], ["count"],
             ">= with 64-char literal: truncated row + 'asdf'"),
            # --- name semantics: not blank-padded, trailing space matters ---
            ("SELECT name 'name string' = name 'name string ';",
             [["f"]], None, None, None, None, None,
             "trailing space is significant (not bpchar)"),
            ("SELECT name 'abc' = 'abc'::text;",
             [["t"]], None, None, None, None, None,
             "name = text cross-type comparison"),
            # --- set-operation type resolution ---
            ("SELECT 'abc'::name UNION SELECT 'def'::text;",
             [["abc"], ["def"]], None, None, None, [25], None,
             "name UNION text resolves to text (OID 25)"),
            ("SELECT 'abc'::name UNION SELECT 'def'::name;",
             [["abc"], ["def"]], None, None, None, [19], None,
             "name UNION name stays name (OID 19)"),
            ("DROP TABLE name_tbl;",
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

        print(f"protocol 58: {passed} passed, {failed} failed")
        return 1 if failed else 0
    finally:
        proc.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
