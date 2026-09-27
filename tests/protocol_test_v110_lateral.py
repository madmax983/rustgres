#!/usr/bin/env python3
r"""v1.10 protocol tests: explicit LATERAL derived tables and VALUES.

Covers (simple protocol):
- comma / CROSS JOIN LATERAL (SELECT ...) correlated per left row
- INNER JOIN LATERAL ... ON (predicate filters pairs)
- LEFT JOIN LATERAL null-extension when the subquery returns no rows
- LATERAL (VALUES ...) seeing the left row
- uncorrelated LATERAL
- nested LATERAL (later item sees earlier sibling)
- multi-row correlated subquery fan-out
- explicit LATERAL on a table function (noise word, any join kind)
- RIGHT/FULL JOIN LATERAL: correlated references are a 42P10 error (PG19:
  "The combining JOIN type must be INNER or LEFT for a LATERAL reference");
  uncorrelated LATERAL is legal
- a relation literally named `lateral` still works (unreserved keyword)
- LATERAL before a parenthesized join is a 42601 syntax error (PG19)

Extended protocol:
- Parse/Describe of a LATERAL (VALUES ...) query returns the same
  column OIDs as execution (int4), proving Describe/execution agree.

Self-starting: launches rustgres on 5590 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5590
SRC_BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
BIN = "/tmp/rg-v110-proto-bin"
DATADIR = "/tmp/rg_proto_v110_lateral"


def msg(t, payload):
    return t + struct.pack("!I", 4 + len(payload)) + payload


def cstr(s):
    return s.encode() + b"\x00"


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("closed")
        d += c
    return d


def read_msg(s):
    t = read_exact(s, 1)
    ln = struct.unpack("!I", read_exact(s, 4))[0]
    return t, read_exact(s, ln - 4)


def connect():
    for _ in range(30):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
            break
        except ConnectionRefusedError:
            time.sleep(0.5)
    else:
        raise RuntimeError("could not connect")
    body = struct.pack("!I", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!I", len(body) + 4) + body)
    while True:
        t, p = read_msg(s)
        if t == b"Z":
            break
    return s


def q(s, sql):
    """Run simple-query SQL, return (rows, error_code)."""
    s.sendall(msg(b"Q", cstr(sql)))
    rows, err = [], None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            pass
        elif t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            j, vals = 2, []
            for _ in range(n):
                ln = struct.unpack("!i", p[j:j+4])[0]; j += 4
                if ln == -1:
                    vals.append(None)
                else:
                    vals.append(p[j:j+ln].decode()); j += ln
            rows.append(tuple(vals))
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return rows, err


def parse_rowdesc(payload):
    """Parse a RowDescription (T) payload into [(name, type_oid)]."""
    n = struct.unpack("!h", payload[:2])[0]
    j, out = 2, []
    for _ in range(n):
        end = payload.index(b"\x00", j)
        name = payload[j:end].decode()
        j = end + 1
        table_oid, col_no = struct.unpack("!ih", payload[j:j+6]); j += 6
        type_oid, = struct.unpack("!i", payload[j:j+4]); j += 4
        j += 2 + 4 + 2  # typlen, typmod, format
        out.append((name, type_oid))
    return out


def extended_describe_oids(s, sql):
    """Parse/Describe via extended protocol; return [(name, type_oid)] or (None, err)."""
    s.sendall(msg(b"P", cstr("") + cstr(sql) + struct.pack("!h", 0)))
    s.sendall(msg(b"D", b"S" + cstr("")))
    s.sendall(msg(b"S", b""))
    desc, err = None, None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            desc = parse_rowdesc(p)
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return desc, err


def main():
    shutil.copy2(SRC_BIN, BIN)
    os.chmod(BIN, 0o755)
    shutil.rmtree(DATADIR, ignore_errors=True)
    os.makedirs(DATADIR)
    srv = subprocess.Popen([BIN, "--data-dir", DATADIR, "--port", str(PORT)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        s = connect()
        fails = []

        def check(label, sql, expect_rows=None, expect_err=None):
            rows, err = q(s, sql)
            if expect_err:
                if err != expect_err:
                    fails.append(f"{label}: expected err {expect_err}, got {err} rows={rows}")
                else:
                    print(f"ok: {label} (err {err})")
            else:
                if err:
                    fails.append(f"{label}: unexpected err {err}")
                elif expect_rows is not None and rows != expect_rows:
                    fails.append(f"{label}: expected {expect_rows}, got {rows}")
                else:
                    print(f"ok: {label} -> {rows}")

        # comma LATERAL correlated subquery
        check("comma lateral correlated",
              "SELECT ss.x, sub.y FROM (VALUES (1), (2)) AS ss(x), LATERAL (SELECT ss.x + 1 AS y) AS sub;",
              expect_rows=[("1", "2"), ("2", "3")])
        # INNER JOIN LATERAL with ON
        check("inner join lateral on",
              "SELECT a, b FROM (VALUES (1), (2)) AS t(a) INNER JOIN LATERAL (SELECT t.a * 10 AS b) AS s ON b > 10;",
              expect_rows=[("2", "20")])
        # LEFT JOIN LATERAL null-extension
        check("left join lateral null-ext",
              "SELECT a, b FROM (VALUES (1), (2)) AS t(a) LEFT JOIN LATERAL (SELECT t.a * 10 AS b WHERE t.a > 1) AS s ON true;",
              expect_rows=[("1", None), ("2", "20")])
        # LATERAL (VALUES ...) sees the left row
        check("lateral values",
              "SELECT a, v FROM (VALUES (1), (2)) AS t(a), LATERAL (VALUES (t.a * 100)) AS v(v);",
              expect_rows=[("1", "100"), ("2", "200")])
        # uncorrelated LATERAL
        check("uncorrelated lateral",
              "SELECT a, b FROM (VALUES (1), (2)) AS t(a), LATERAL (SELECT 42 AS b) AS s;",
              expect_rows=[("1", "42"), ("2", "42")])
        # nested LATERAL sees earlier sibling
        check("nested lateral",
              "SELECT a, x, y FROM (VALUES (1)) AS t(a), LATERAL (SELECT t.a + 1 AS x) AS s1, LATERAL (SELECT s1.x + 1 AS y) AS s2;",
              expect_rows=[("1", "2", "3")])
        # multi-row correlated fan-out
        check("multi-row correlated",
              "SELECT s.i, t.j FROM (VALUES (1), (2)) AS s(i), LATERAL (SELECT s.i * 10 AS j UNION ALL SELECT s.i * 100) AS t(j);",
              expect_rows=[("1", "10"), ("1", "100"), ("2", "20"), ("2", "200")])
        # explicit LATERAL on a table function (noise word)
        check("lateral func explicit",
              "SELECT a, g FROM (VALUES (2), (3)) AS t(a), LATERAL generate_series(1, t.a) AS g;",
              expect_rows=[("2", "1"), ("2", "2"), ("3", "1"), ("3", "2"), ("3", "3")])
        # RIGHT/FULL JOIN LATERAL: PG19 forbids CORRELATED lateral
        # references ("The combining JOIN type must be INNER or LEFT for
        # a LATERAL reference"); uncorrelated LATERAL is legal.
        check("right join lateral correlated -> 42P10",
              "SELECT a, b FROM (VALUES (1)) AS t(a) RIGHT JOIN LATERAL (SELECT t.a * 10 AS b) AS s ON true;",
              expect_err="42P10")
        check("full join lateral correlated -> 42P10",
              "SELECT a, b FROM (VALUES (1)) AS t(a) FULL JOIN LATERAL (SELECT t.a * 10 AS b) AS s ON true;",
              expect_err="42P10")
        check("right join lateral uncorrelated",
              "SELECT a, b FROM (VALUES (1)) AS t(a) RIGHT JOIN LATERAL (SELECT 42 AS b) AS s ON true;",
              expect_rows=[("1", "42")])
        # scalar subquery inside LATERAL VALUES
        check("scalar subquery in lateral values",
              "SELECT s.i, val.x FROM (VALUES (1), (2)) AS s(i), LATERAL (VALUES ((SELECT s.i + 1)), (s.i + 101)) AS val(x);",
              expect_rows=[("1", "2"), ("1", "102"), ("2", "3"), ("2", "103")])
        # relation literally named lateral
        check("create table lateral",
              "CREATE TABLE lateral (x int); INSERT INTO lateral VALUES (7);")
        check("table named lateral",
              "SELECT x FROM lateral;",
              expect_rows=[("7",)])
        # LATERAL before a parenthesized join: PG19 syntax error
        check("lateral parenthesized join",
              "SELECT * FROM (VALUES (1)) AS t(a), LATERAL ((SELECT 1) CROSS JOIN (SELECT 2)) AS s;",
              expect_err="42601")

        # Extended protocol: Describe OIDs for LATERAL (VALUES ...)
        desc, err = extended_describe_oids(
            s,
            "SELECT s.i, val.x FROM (VALUES (1), (2)) AS s(i), LATERAL (VALUES (s.i + 1)) AS val(x)")
        if err:
            fails.append(f"describe lateral values: unexpected err {err}")
        elif desc != [("i", 23), ("x", 23)]:
            fails.append(f"describe lateral values: expected [('i', 23), ('x', 23)], got {desc}")
        else:
            print(f"ok: describe lateral values oids -> {desc}")

        desc, err = extended_describe_oids(
            s,
            "SELECT sub.y FROM (VALUES (1)) AS ss(x), LATERAL (SELECT ss.x || '!' AS y) AS sub")
        if err:
            fails.append(f"describe lateral text: unexpected err {err}")
        elif desc != [("y", 25)]:
            fails.append(f"describe lateral text: expected [('y', 25)], got {desc}")
        else:
            print(f"ok: describe lateral text oids -> {desc}")

        s.close()
        if fails:
            print("\nFAILURES:")
            for f in fails:
                print("  " + f)
            return 1
        print("\nAll v1.10 protocol tests passed.")
        return 0
    finally:
        srv.terminate()

if __name__ == "__main__":
    sys.exit(main())
