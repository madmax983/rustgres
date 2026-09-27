#!/usr/bin/env python3
r"""v1.09 protocol tests: user-defined function DDL & execution parity.

Covers:
- CREATE FUNCTION ... PARALLEL {UNSAFE|RESTRICTED|SAFE} (planner hint,
  parsed and validated like COST; invalid value is 42601).
- User-defined RETURNS SETOF functions in the SELECT targetlist
  (PG19 ProjectSet fan-out: one row per element).
- ALTER FUNCTION ... {VOLATILE|STABLE|IMMUTABLE} (updates volatility;
  unknown function is 42883).

Self-starting: launches rustgres on 5453 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5453
SRC_BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
BIN = "/tmp/rg-v109-proto-bin"
DATADIR = "/tmp/rg_proto_v109_functions"


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
            import re
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return rows, err


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

        # PARALLEL option
        check("parallel safe",
              "CREATE FUNCTION pf1(int) RETURNS int LANGUAGE sql IMMUTABLE PARALLEL SAFE AS 'SELECT $1 * 2';")
        check("parallel restricted",
              "CREATE FUNCTION pf2(int) RETURNS int LANGUAGE sql PARALLEL RESTRICTED AS 'SELECT $1';")
        check("parallel unsafe",
              "CREATE FUNCTION pf3(int) RETURNS int LANGUAGE sql PARALLEL UNSAFE AS 'SELECT $1';")
        check("parallel bogus", 
              "CREATE FUNCTION pf4(int) RETURNS int LANGUAGE sql PARALLEL BOGUS AS 'SELECT $1';",
              expect_err="42601")
        check("pf1(21)", "SELECT pf1(21);", expect_rows=[("42",)])

        # User SRF in targetlist
        check("create sillysrf",
              "CREATE FUNCTION sillysrf(int) RETURNS SETOF int AS 'values (1),(10),(2),($1)' LANGUAGE sql IMMUTABLE;")
        check("sillysrf(42)", "SELECT sillysrf(42);",
              expect_rows=[("1",), ("10",), ("2",), ("42",)])
        check("sillysrf(-1) order by",
              "SELECT sillysrf(-1) ORDER BY 1;",
              expect_rows=[("-1",), ("1",), ("2",), ("10",)])
        check("drop sillysrf", "DROP FUNCTION sillysrf(int);")

        # ALTER FUNCTION volatility
        check("create av",
              "CREATE FUNCTION av(int) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT $1';")
        check("alter av immutable", "ALTER FUNCTION av(int) IMMUTABLE;")
        check("alter av stable", "ALTER FUNCTION av(int) STABLE;")
        check("alter av volatile", "ALTER FUNCTION av(int) VOLATILE;")
        check("alter nonexistent",
              "ALTER FUNCTION nonexistent(int) IMMUTABLE;",
              expect_err="42883")
        check("av(5)", "SELECT av(5);", expect_rows=[("5",)])

        s.close()
        if fails:
            print("\nFAILURES:")
            for f in fails:
                print("  " + f)
            return 1
        print("\nAll v1.09 protocol tests passed.")
        return 0
    finally:
        srv.terminate()

if __name__ == "__main__":
    sys.exit(main())
