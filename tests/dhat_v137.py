#!/usr/bin/env python3
"""dhat_v137.py — DHAT heap profile for the v1.37 plan-fold-memo paths.

The memo trades per-row argument evaluation + eligibility walks for one
HashMap<*const Expr, Value> per statement; this profile captures the
heap shape of a fold-heavy workload (memo insert + hits, nested
body-internal memos, CTE-shadow ineligible path).
"""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5596
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "CREATE FUNCTION d7_add(x int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT $1 + 1'",
    "CREATE FUNCTION d7_inner(x int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT $1 * 10'",
    "CREATE FUNCTION d7_outer() RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT sum(d7_inner(v)) FROM (VALUES (1),(2),(3)) t(v)'",
    "CREATE TABLE d7_tab(v int)",
    "INSERT INTO d7_tab VALUES (100)",
    "CREATE FUNCTION d7_sh(x int) RETURNS int IMMUTABLE LANGUAGE sql AS 'SELECT v FROM d7_tab'",
    "SELECT d7_add(41) FROM generate_series(1, 3000) g",
    "SELECT d7_add(v), d7_add(v + 1) FROM (SELECT g AS v FROM generate_series(1, 1000) g) t",
    "SELECT d7_outer() FROM generate_series(1, 500) g",
    "SELECT x.f1, d7_sh(1) FROM (WITH d7_tab AS (SELECT 7 AS v) SELECT d7_sh(1) AS f1) x",
    "SELECT d7_add(d7_inner(g)) FROM generate_series(1, 1000) g",
    "DROP FUNCTION d7_add(int)",
    "DROP FUNCTION d7_inner(int)",
    "DROP FUNCTION d7_outer()",
    "DROP FUNCTION d7_sh(int)",
    "DROP TABLE d7_tab",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat137_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "dhat.log")
    proc = subprocess.Popen(
        [VG, "--tool=dhat", f"--log-file={log}",
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
            print("server did not start under dhat"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
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

        def q(sql):
            s.sendall(b"Q" + struct.pack("!i", 5 + len(sql)) + sql.encode() + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z":
                    return

        for sql in STMTS:
            q(sql)
        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()

    print("dhat_v137: done, log at", log)
    try:
        with open(log) as f:
            txt = f.read()
        for pat in (r"Total:\s+([0-9,]+ bytes[^\n]*)",
                    r"Maximum live:[^\n]*",
                    r"At end of run[^\n]*"):
            m = re.search(pat, txt)
            if m:
                print("DHAT:", m.group(0).strip()[:120])
    except Exception as e:
        print("dhat log parse failed:", e)
    return 0


if __name__ == "__main__":
    sys.exit(main())
