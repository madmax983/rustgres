#!/usr/bin/env python3
r"""v1.08 protocol tests: EXPLAIN (COSTS ...) PG19 text-format parity.

Grounded in PostgreSQL REL_19_STABLE:
- src/backend/commands/explain.c (NewExplainState defaults costs=true;
  options processed sequentially, last-wins for duplicates;
  `unrecognized EXPLAIN option` is 42601)
- src/backend/commands/explain_state.c / explain_format.c
  (ExplainIndentText: indent*2 spaces; `->  ` arrow; es->indent += 2
  after arrow, ++ after node name; properties at indent*2 spaces)
- src/backend/utils/adt/ruleutils.c (expression deparsing:
  binary ops parenthesized, AND/OR/NOT forms, type coercion labels)

Self-starting: launches rustgres on 5452 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5452
SRC_BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
BIN = "/tmp/rg-v108-proto-bin"
DATADIR = "/tmp/rg_proto_v108_costs"


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
    """Run simple-query SQL, return (rows, colnames, error)."""
    s.sendall(msg(b"Q", cstr(sql)))
    rows, colnames, err = [], [], None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            n = struct.unpack("!h", p[:2])[0]
            j = 2
            for _ in range(n):
                e = p.index(b"\x00", j)
                colnames.append(p[j:e].decode())
                j = e + 1
                j += 18  # skip fixed fields
        elif t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            j = 2
            cols = []
            for _ in range(n):
                cl = struct.unpack("!i", p[j:j+4])[0]
                j += 4
                if cl < 0:
                    cols.append(None)
                else:
                    cols.append(p[j:j+cl].decode())
                    j += cl
            rows.append(cols)
        elif t == b"E":
            # extract message and code
            err = {}
            j = 0
            while j < len(p) - 1:
                f = chr(p[j])
                e = p.index(b"\x00", j+1)
                err[f] = p[j+1:e].decode()
                j = e + 1
        elif t == b"Z":
            break
    return rows, colnames, err


def check(name, cond, detail=""):
    if not cond:
        print(f"FAIL: {name} {detail}")
        return False
    print(f"ok: {name}")
    return True


def main():
    shutil.copy2(SRC_BIN, BIN)
    os.chmod(BIN, 0o755)
    shutil.rmtree(DATADIR, ignore_errors=True)
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", DATADIR],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    ok = True
    try:
        s = connect()
        # Setup
        for sql in [
            "CREATE TABLE t1 (a int, b text)",
            "CREATE TABLE t2 (x int, y int)",
            "CREATE INDEX i1 ON t1 (a)",
            "INSERT INTO t1 VALUES (1,'x'),(2,'y')",
            "INSERT INTO t2 VALUES (1,10),(2,20)",
        ]:
            rows, _, err = q(s, sql)
            if err:
                print(f"setup failed: {sql}: {err}")
                return 1

        # 1. Result column is exactly "QUERY PLAN"
        rows, cols, err = q(s, "EXPLAIN (COSTS OFF) SELECT 1")
        ok &= check("result column name", cols == ["QUERY PLAN"], f"got {cols}")

        # 2. COSTS OFF omits (rows=N); COSTS ON (default) keeps it
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1")
        ok &= check("costs-off no rows=", all("(rows=" not in r[0] for r in rows))
        rows, _, _ = q(s, "EXPLAIN SELECT * FROM t1")
        ok &= check("default has rows=", any("(rows=" in r[0] for r in rows))
        rows, _, _ = q(s, "EXPLAIN (COSTS ON) SELECT * FROM t1")
        ok &= check("costs-on has rows=", any("(rows=" in r[0] for r in rows))

        # 3. Boolean forms
        for sql, expect_off in [
            ("EXPLAIN (COSTS OFF) SELECT 1", True),
            ("EXPLAIN (COSTS FALSE) SELECT 1", True),
            ("EXPLAIN (COSTS 0) SELECT 1", True),
            ("EXPLAIN (COSTS) SELECT 1", False),
            ("EXPLAIN (COSTS ON) SELECT 1", False),
            ("EXPLAIN (COSTS TRUE) SELECT 1", False),
            ("EXPLAIN (COSTS 1) SELECT 1", False),
        ]:
            rows, _, _ = q(s, sql)
            has_rows = any("(rows=" in r[0] for r in rows)
            ok &= check(f"boolean form {sql}", has_rows == (not expect_off))

        # 4. Duplicate last-wins
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF, COSTS ON) SELECT * FROM t1")
        ok &= check("dup last-wins ON", any("(rows=" in r[0] for r in rows))
        rows, _, _ = q(s, "EXPLAIN (COSTS ON, COSTS OFF) SELECT * FROM t1")
        ok &= check("dup last-wins OFF", all("(rows=" not in r[0] for r in rows))

        # 5. Unknown option -> 42601
        _, _, err = q(s, "EXPLAIN (FOOBAR) SELECT 1")
        ok &= check("unknown option 42601",
                    err and err.get("C") == "42601"
                    and "unrecognized EXPLAIN option" in err.get("M", ""))

        # 6. Invalid boolean -> error
        _, _, err = q(s, "EXPLAIN (COSTS foo) SELECT 1")
        ok &= check("invalid boolean errors", err is not None)

        # 7. Index Scan exact text and indentation
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a = 42")
        plan = [r[0] for r in rows]
        ok &= check("index scan line", plan[0] == "Index Scan using i1 on t1", f"got {plan[0]!r}")
        ok &= check("index cond indent", plan[1] == "  Index Cond: (a = 42)", f"got {plan[1]!r}")

        # 8. Residual filter with type coercion
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a = 42 AND b = 'hello'")
        plan = [r[0] for r in rows]
        ok &= check("residual filter", plan[2] == "  Filter: (b = 'hello'::text)", f"got {plan[2]!r}")

        # 9. Alias rendering
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1 t WHERE t.a = 1")
        plan = [r[0] for r in rows]
        ok &= check("alias on scan", plan[0] == "Index Scan using i1 on t1 t", f"got {plan[0]!r}")

        # 10. Join with Join Filter and exact PG indentation
        # (t1.a > 0 is indexable, so the left child is an Index Scan with
        # the predicate as Index Cond — the planner's correct choice).
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1 a, t2 b WHERE a.a = b.x AND a.a > 0")
        plan = [r[0] for r in rows]
        ok &= check("join top node", plan[0] == "Nested Loop", f"got {plan[0]!r}")
        ok &= check("join filter", plan[1] == "  Join Filter: (a.a = b.x)", f"got {plan[1]!r}")
        ok &= check("join child arrow", plan[2] == "  ->  Index Scan using i1 on t1 a", f"got {plan[2]!r}")
        ok &= check("child index-cond indent", plan[3] == "        Index Cond: (a > 0)", f"got {plan[3]!r}")

        # 11. Sort Key
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1 ORDER BY a + 1")
        plan = [r[0] for r in rows]
        ok &= check("sort node", plan[0] == "Sort")
        ok &= check("sort key", plan[1] == "  Sort Key: (t1.a + 1)", f"got {plan[1]!r}")

        # 12. Bare Limit
        rows, _, _ = q(s, "EXPLAIN (COSTS OFF) SELECT * FROM t1 LIMIT 5")
        plan = [r[0] for r in rows]
        ok &= check("bare limit", plan[0] == "Limit", f"got {plan[0]!r}")

        s.close()
    finally:
        proc.terminate()
        proc.wait()
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
