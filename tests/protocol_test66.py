#!/usr/bin/env python3
r"""v0.65 protocol tests: DML target semantics.

RED on the inherited base (b992475: v0.64), GREEN after v0.65.

Three PG19 behaviors, all currently broken:

1. Real SERIAL: `id SERIAL PRIMARY KEY` must create a backing sequence
   `<table>_<column>_seq`, mark the column NOT NULL, and wire
   `DEFAULT nextval('<table>_<column>_seq')` so omitted columns get fresh
   ids. (v0.64 and earlier mapped `serial` to a bare integer: omitting the
   column raised 23502.) smallserial/bigserial likewise.

2. INSERT first-N rule (PG INSERT docs): with no column list,
   `INSERT INTO t VALUES (v1, v2)` on a 3-column table targets the first
   two columns and the rest take defaults. (v0.64 demanded an exact
   count -> 42601.)

3. DELETE table alias: `DELETE FROM t AS dt WHERE dt.a > ...`.
   (v0.64: 42601.)

Expected PG19 results below were taken from the documented behaviors:
serial creates `<table>_<column>_seq` starting at 1; INSERT's first-N
rule; DELETE's alias clause.
"""
import socket, struct, sys

PORT = 5433

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d

def read_msg(s):
    hdr = read_exact(s, 5)
    typ, ln = struct.unpack("!cI", hdr)
    return typ, read_exact(s, ln - 4)

def startup(s):
    msg = struct.pack("!II", 196608, 0) + b"user\x00test\x00\x00"
    s.sendall(struct.pack("!I", len(msg) + 4) + msg)
    while True:
        typ, _ = read_msg(s)
        if typ == b"Z":
            break

def parse_fields(payload, n):
    """Split a RowDescription/DataRow payload into field byte-strings."""
    pos = 0
    out = []
    for _ in range(n):
        end = payload.find(b"\x00", pos)
        out.append(payload[pos:end])
        pos = end + 1
    return out, pos

def q(s, sql):
    """Run sql; return ('ok', rows) or ('err', sqlstate). rows = list of lists of str|None."""
    s.sendall(struct.pack("!cI", b"Q", len(sql) + 5) + sql.encode() + b"\x00")
    rows = []
    nfields = 0
    while True:
        typ, payload = read_msg(s)
        if typ == b"T":
            nfields = struct.unpack("!H", payload[:2])[0]
        elif typ == b"D":
            nfields_msg = struct.unpack("!H", payload[:2])[0]
            pos = 2
            row = []
            for _ in range(nfields_msg):
                ln = struct.unpack("!i", payload[pos:pos+4])[0]
                pos += 4
                if ln < 0:
                    row.append(None)
                else:
                    row.append(payload[pos:pos+ln].decode())
                    pos += ln
            rows.append(row)
        elif typ == b"E":
            pos = 0
            code = None
            while pos < len(payload):
                f = payload[pos:pos+1]
                pos += 1
                if f == b"\x00":
                    break
                end = payload.find(b"\x00", pos)
                if f == b"C":
                    code = payload[pos:end].decode()
                pos = end + 1
            # drain to ReadyForQuery
            while True:
                t2, _ = read_msg(s)
                if t2 == b"Z":
                    break
            return ("err", code)
        elif typ == b"Z":
            break
    return ("ok", rows)

def expect_ok(s, sql, want_rows=None, label=None):
    st, rows = q(s, sql)
    assert st == "ok", f"{label or sql}: expected success, got {st} {rows}"
    if want_rows is not None:
        assert rows == want_rows, f"{label or sql}: expected {want_rows}, got {rows}"
    print(f"PASS: {label or sql}")

def expect_err(s, sql, code, label=None):
    st, got = q(s, sql)
    assert st == "err" and got == code, f"{label or sql}: expected error {code}, got {st} {got}"
    print(f"PASS: {label or sql} -> {code}")

def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    try:
        startup(s)
        # --- real SERIAL ---
        expect_ok(s, "CREATE TABLE t66a (id SERIAL PRIMARY KEY, a INT, b TEXT)")
        expect_ok(s, "INSERT INTO t66a (a) VALUES (10)")
        expect_ok(s, "SELECT id, a FROM t66a", [["1", "10"]], "serial default fills id=1")
        expect_ok(s, "INSERT INTO t66a (a, b) VALUES (20, 'x'), (30, 'y')")
        expect_ok(s, "SELECT id FROM t66a ORDER BY id", [["1"], ["2"], ["3"]],
                    "serial ids advance 1,2,3")
        expect_ok(s, "SELECT currval('t66a_id_seq')", [["3"]],
                    "backing sequence named t66a_id_seq")
        expect_ok(s, "CREATE TABLE t66c (s SMALLSERIAL, b BIGSERIAL, v INT DEFAULT 7)")
        expect_ok(s, "INSERT INTO t66c (v) VALUES (1)")
        expect_ok(s, "SELECT s, b, v FROM t66c", [["1", "1", "1"]],
                    "smallserial/bigserial defaults")
        expect_err(s, "INSERT INTO t66a (id, a) VALUES (NULL, 1)", "23502",
                   "serial column is NOT NULL")
        # --- PG19: explicit DEFAULT on a serial column is 42601 ---
        expect_err(s, "CREATE TABLE t66d (id SERIAL DEFAULT 5)", "42601",
                   "SERIAL DEFAULT -> multiple default values")
        expect_err(s, "ALTER TABLE t66c ADD COLUMN q BIGSERIAL DEFAULT 9", "42601",
                   "ADD COLUMN bigserial DEFAULT -> multiple default values")
        # --- PG19: type-specific sequence MAXVALUE (sequence.c) ---
        expect_ok(s, "SELECT setval('t66c_s_seq', 32767, false)", [["32767"]])
        expect_ok(s, "SELECT nextval('t66c_s_seq')", [["32767"]],
                    "smallserial seq max 32767")
        expect_err(s, "SELECT nextval('t66c_s_seq')", "55000",
                   "smallserial seq exhausts at 32767")
        expect_ok(s, "SELECT setval('t66c_b_seq', 2147483648, false)", [["2147483648"]])
        expect_ok(s, "SELECT nextval('t66c_b_seq')", [["2147483648"]],
                    "bigserial seq max is int8-range, not int4-capped")
        # --- INSERT first-N rule ---
        expect_ok(s, "CREATE TABLE t66b (x INT, y INT, z TEXT DEFAULT 'dflt')")
        expect_ok(s, "INSERT INTO t66b VALUES (1, 2)")
        expect_ok(s, "SELECT x, y, z FROM t66b", [["1", "2", "dflt"]],
                    "first-N: z takes its default")
        expect_err(s, "INSERT INTO t66b VALUES (4, 5, 6, 7)", "42601",
                   "too many values still 42601")
        expect_err(s, "INSERT INTO t66b (x) VALUES (1, 2)", "42601",
                   "explicit column list keeps exact-count rule")
        # --- DELETE alias ---
        expect_ok(s, "DELETE FROM t66a AS dt WHERE dt.a > 15", None,
                    "DELETE with AS alias")
        expect_ok(s, "SELECT count(*) FROM t66a", [["1"]], "alias delete removed 2 rows")
        expect_ok(s, "DELETE FROM t66a dt WHERE dt.a = 10")
        expect_ok(s, "SELECT count(*) FROM t66a", [["0"]], "bare alias form works")
        # --- owned sequence dropped with its table ---
        expect_ok(s, "DROP TABLE t66a")
        expect_err(s, "SELECT nextval('t66a_id_seq')", "42P01",
                   "serial sequence dropped with table")
        # --- user sequence with explicit DEFAULT nextval() survives ---
        expect_ok(s, "CREATE SEQUENCE t66u_seq")
        expect_ok(s, "CREATE TABLE t66u (id INT DEFAULT nextval('t66u_seq'), v INT)")
        expect_ok(s, "DROP TABLE t66u")
        expect_ok(s, "SELECT nextval('t66u_seq')", [["1"]],
                    "user sequence survives DROP TABLE")
        expect_ok(s, "DROP SEQUENCE t66u_seq")
        expect_ok(s, "DROP TABLE t66b")
        expect_ok(s, "DROP TABLE t66c")
        print("protocol_test66: all DML target-semantics checks passed")
    finally:
        s.close()

if __name__ == "__main__":
    main()
