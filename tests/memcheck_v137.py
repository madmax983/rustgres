#!/usr/bin/env python3
"""memcheck_v137.py — run the v1.37 plan-fold-memo paths under valgrind memcheck.

Exercises: call-site memo insert + repeated hits across rows (multi-site,
zero-arg, named-arg, body-local CTE), caller-CTE-shadow ineligible path
(both evaluation orders), VOLATILE/STABLE never-memoized, nested
body-internal calls, and the error-never-memoized path.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5549
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "CREATE FUNCTION m7_add(x int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT $1 + 1'",
    "CREATE FUNCTION m7_vol() RETURNS float8 VOLATILE LANGUAGE sql AS 'SELECT random()'",
    "CREATE SEQUENCE m7_s",
    "CREATE FUNCTION m7_sv() RETURNS int VOLATILE LANGUAGE sql AS 'SELECT nextval(''m7_s'')'",
    "CREATE FUNCTION m7_st() RETURNS int STABLE LANGUAGE sql AS 'SELECT nextval(''m7_s'')'",
    "CREATE FUNCTION m7_inner(x int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT $1 * 10'",
    "CREATE FUNCTION m7_outer() RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT sum(m7_inner(v)) FROM (VALUES (1),(2),(3)) t(v)'",
    "CREATE FUNCTION m7_zero() RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT 99'",
    "CREATE FUNCTION m7_add2(x int, y int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT $1 + $2'",
    "CREATE FUNCTION m7_cte() RETURNS int IMMUTABLE LANGUAGE sql AS 'WITH w AS (SELECT 42 AS v) SELECT v FROM w'",
    "CREATE TABLE m7_tab(v int)",
    "INSERT INTO m7_tab VALUES (100)",
    "CREATE FUNCTION m7_sh(x int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT v FROM m7_tab'",
    "CREATE FUNCTION m7_err() RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT 1/0'",
    # memo insert + hits across rows, two call sites in one statement
    "SELECT m7_add(41) FROM generate_series(1, 500) g",
    "SELECT m7_add(v), m7_add(v) FROM (VALUES (1),(2),(1),(3)) AS t(v)",
    # never-memoized paths
    "SELECT m7_vol() FROM generate_series(1, 20) g",
    "SELECT m7_sv(), m7_sv() FROM generate_series(1, 10) g",
    "SELECT m7_st() FROM generate_series(1, 10) g",
    # nested body-internal calls, zero-arg, named-arg, body-local CTE
    "SELECT m7_outer() FROM generate_series(1, 50) g",
    "SELECT m7_zero(), m7_zero() FROM generate_series(1, 100) g",
    "SELECT m7_add2(y => 2, x => 40) FROM generate_series(1, 100) g",
    "SELECT m7_cte() FROM generate_series(1, 100) g",
    # caller-CTE-shadow ineligible path, both evaluation orders
    "SELECT x.f1, m7_sh(1) FROM (WITH m7_tab AS (SELECT 7 AS v) SELECT m7_sh(1) AS f1) x",
    "SELECT m7_sh(1), x.f1 FROM (WITH m7_tab AS (SELECT 7 AS v) SELECT m7_sh(1) AS f1) x",
    "SELECT m7_add(m7_inner(3)) FROM generate_series(1, 50) g",
    # error path: raises 22012, never memoized
    "SELECT m7_err()",
    "DROP FUNCTION m7_add(int)",
    "DROP FUNCTION m7_vol()",
    "DROP FUNCTION m7_sv()",
    "DROP FUNCTION m7_st()",
    "DROP FUNCTION m7_inner(int)",
    "DROP FUNCTION m7_outer()",
    "DROP FUNCTION m7_zero()",
    "DROP FUNCTION m7_add2(int, int)",
    "DROP FUNCTION m7_cte()",
    "DROP FUNCTION m7_sh(int)",
    "DROP FUNCTION m7_err()",
    "DROP TABLE m7_tab",
    "DROP SEQUENCE m7_s",
    "checkpoint",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc137_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not start under valgrind"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d

        def msg():
            t = rd(1)
            ln = struct.unpack("!i", rd(4))[0]
            return t, rd(ln - 4)

        while True:
            t, _ = msg()
            if t == b"Z":
                break

        def simple(q):
            qb = q.encode()
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            got_err = None
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            got_err = p[i + 1:e].decode()
                        i = e + 1
                elif t == b"Z":
                    break
            return got_err

        nfail = 0
        err_codes = []
        for q in STMTS:
            err = simple(q)
            if err:
                nfail += 1
                err_codes.append(err)
                print(f"SQL ERR {err}: {q[:60]}")
        print(f"statements with SQL errors: {nfail} {err_codes}")
        # Exactly one SQL error is expected: m7_err() -> 22012.
        if nfail != 1 or err_codes != ["22012"]:
            print("UNEXPECTED SQL error pattern")
            sys.exit(3)
        s.close()
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=60)
        except Exception:
            proc.kill()
    with open(log) as f:
        txt = f.read()
    errors = 0
    for line in txt.splitlines():
        if "ERROR SUMMARY" in line:
            print(line.strip())
            try:
                errors = int(line.split()[3])
            except Exception:
                pass
    if errors:
        print("--- first 30 error lines ---")
        n = 0
        for line in txt.splitlines():
            if "valgrind" in line.lower() or "Invalid" in line or "uninitialised" in line:
                print(line.rstrip())
                n += 1
                if n >= 30:
                    break
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
