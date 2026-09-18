#!/usr/bin/env python3
"""Protocol 56: CASE expressions (simple and searched), PG19 semantics.

v0.55 implements PostgreSQL 19 `CASE` (gram.y `case_expr`):

* Searched form: `CASE WHEN cond THEN result ... [ELSE result] END`.
  WHEN conditions must be boolean-or-NULL (NULL counts as not true);
  anything else is 42804, like WHERE.
* Simple form: `CASE operand WHEN key THEN result ... [ELSE result] END`.
  The operand is evaluated once; keys use regular `=` semantics, so NULL
  never matches (it is not `IS NOT DISTINCT FROM`). Unknown (text)
  literals coerce to the operand type, like PG's unknown-type coercion.
* Missing ELSE yields NULL.
* Arms short-circuit: later arms -- and their errors -- are not
  evaluated (`CASE WHEN 1=0 THEN 1/0 ...` does not raise).
* Result type is the common supertype of all result arms (+ ELSE),
  unknown literals not constraining (PG's select_common_type); the
  taken arm is coerced to it.
* Unaliased output column is named `case` (PG's FigureColname).

RED on v0.54 (base bf7830b): every CASE below raises 42601
(syntax error at or near "case").
GREEN on v0.55: all cases pass.
"""
import socket, struct, subprocess, time, os

PORT = 5556
DATA_DIR = "/tmp/rg56proto"
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
    "CREATE TABLE t56 (a int, b text)",
    "INSERT INTO t56 VALUES (1, 'one'), (2, 'two'), (NULL, 'nil')",
]


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg56proto.log", "w")
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
        # OIDs: 16=bool, 23=int4, 25=text, 1700=numeric
        cases = [
            # --- searched CASE basics ---
            ("SELECT CASE WHEN 1=0 THEN 'no' WHEN 1=1 THEN 'yes' ELSE '?' END",
             [["yes"]], None, None, None, [25], ["case"],
             "searched CASE picks the true arm (v0.54: 42601)"),
            ("SELECT CASE WHEN false THEN 1 END",
             [[None]], None, None, None, [23], ["case"],
             "missing ELSE yields NULL"),
            ("SELECT CASE WHEN true THEN 1 WHEN true THEN 2 ELSE 3 END",
             [["1"]], None, None, None, [23], ["case"],
             "first true arm wins"),
            # --- short-circuit: untaken arms' errors never fire ---
            ("SELECT CASE WHEN 1=0 THEN 1/0 WHEN 1=1 THEN 1 ELSE 2/0 END",
             [["1"]], None, None, None, [23], ["case"],
             "searched CASE short-circuits (v0.54: 42601)"),
            ("SELECT CASE 1 WHEN 0 THEN 1/0 WHEN 1 THEN 1 ELSE 2/0 END",
             [["1"]], None, None, None, [23], ["case"],
             "simple CASE short-circuits (v0.54: 42601)"),
            # --- simple CASE ---
            ("SELECT CASE 'a' WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 3 END",
             [["1"]], None, None, None, [23], ["case"],
             "simple CASE on text (v0.54: 42601)"),
            ("SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END",
             [["two"]], None, None, None, [25], ["case"],
             "simple CASE on int, no ELSE"),
            ("SELECT CASE NULL WHEN NULL THEN 1 ELSE 2 END",
             [["2"]], None, None, None, [23], ["case"],
             "NULL operand never matches (not IS NOT DISTINCT FROM)"),
            ("SELECT CASE 1 WHEN NULL THEN 'null' ELSE 'other' END",
             [["other"]], None, None, None, [25], ["case"],
             "NULL WHEN key never matches"),
            ("SELECT CASE 1 WHEN '1' THEN 'one' ELSE 'other' END",
             [["one"]], None, None, None, [25], ["case"],
             "unknown literal key coerces to operand type"),
            # --- result type unification ---
            ("SELECT CASE WHEN false THEN 1 ELSE 2.5 END",
             [["2.5"]], None, None, None, [1700], ["case"],
             "int/numeric arms unify to numeric"),
            ("SELECT CASE WHEN true THEN 1 ELSE 2.5 END",
             [["1"]], None, None, None, [1700], ["case"],
             "taken int arm coerced to numeric"),
            ("SELECT CASE WHEN true THEN 'x' ELSE 'y' END",
             [["x"]], None, None, None, [25], ["case"],
             "all-unknown arms resolve to text"),
            # --- semantic errors ---
            ("SELECT CASE WHEN 1 THEN 2 END",
             None, None, "42804", None, None, None,
             "non-boolean WHEN: 42804 (v0.54: 42601)"),
            ("SELECT CASE WHEN 'x' THEN 2 END",
             None, None, "42804", None, None, None,
             "text WHEN: 42804"),
            ("SELECT CASE WHEN true THEN 1 ELSE now() END",
             None, None, "42804", None, None, None,
             "incompatible arm types: 42804"),
            # --- CASE over table data ---
            ("SELECT a, CASE WHEN a IS NULL THEN 'nil' WHEN a = 1 THEN 'one' ELSE 'many' END AS lbl FROM t56 ORDER BY a",
             [["1", "one"], ["2", "many"], [None, "nil"]], None, None, None,
             [23, 25], ["a", "lbl"],
             "searched CASE over rows with NULL"),
            ("SELECT CASE a WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END FROM t56 ORDER BY a",
             [["one"], ["two"], ["other"]], None, None, None, [25], ["case"],
             "simple CASE over rows (NULL sorts last, hits ELSE)"),
            ("SELECT CASE WHEN count(*) > 2 THEN 'many' ELSE 'few' END FROM t56",
             [["many"]], None, None, None, [25], ["case"],
             "aggregate inside CASE (grouped path)"),
            # --- nested CASE ---
            ("SELECT CASE WHEN true THEN CASE WHEN false THEN 1 ELSE 2 END ELSE 3 END",
             [["2"]], None, None, None, [23], ["case"],
             "nested searched CASE"),
            ("SELECT CASE CASE WHEN true THEN 1 ELSE 2 END WHEN 1 THEN 'one' ELSE 'other' END",
             [["one"]], None, None, None, [25], ["case"],
             "CASE as simple-CASE operand"),
            # --- CASE in UPDATE (pg_regress case.sql pattern) ---
            ("UPDATE t56 SET a = CASE WHEN a >= 2 THEN (-a) ELSE (2 * a) END WHERE a IS NOT NULL",
             None, "UPDATE 2", None, None, None, None,
             "UPDATE with CASE + unary minus in arms"),
            ("SELECT a FROM t56 WHERE a IS NOT NULL ORDER BY a",
             [["-2"], ["2"]], None, None, None, None, None,
             "UPDATE...CASE wrote the right values"),
            # --- syntax errors stay syntax errors ---
            ("SELECT CASE WHEN true THEN 1",
             None, None, "42601", None, None, None,
             "missing END: 42601"),
            ("SELECT CASE ELSE 1 END",
             None, None, "42601", None, None, None,
             "missing WHEN: 42601"),
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
                print(f"ok: {desc}")
            else:
                failed += 1
                print(f"FAIL: {desc}\n  sql={sql}\n  {why}")
        print(f"\nprotocol56: {passed} passed, {failed} failed")
        raise SystemExit(1 if failed else 0)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    main()
