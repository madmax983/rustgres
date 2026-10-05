#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v1.72 new paths.

Starts the server under `valgrind --tool=memcheck --leak-check=full`,
exercises the v1.72 DP EC-canonical join-filter paths:
- 3-way sj EC statement (the flip: (t1.a = t3.b) / (t1.a = t2.b))
- 4-way comma join DP with EC (no-op canonical case)
- 2-way sj EC (v1.71 path regression — shares pg_ec_join_filter_text)
- unaliased 3-way self-join (self-join guard fail-closed)
- executor sanity (EXPLAIN-text-only change must not affect execution)

Exit 0 iff: 0 ERROR SUMMARY errors and no definite/possible leaks.
"""
import socket, struct, subprocess, time, os, sys, re, shutil

PORT = 5572
DATA_DIR = os.path.expanduser("~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v172/vg172-data")
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = os.path.expanduser("~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v172/valgrind_v172.log")
VALGRIND = os.environ.get("VALGRIND_BIN", os.path.expanduser("~/workspace/valgrind-local/valgrind"))


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d


def connect():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
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
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"D":
            rows.append(b)
        elif t == b"E":
            err = b
        elif t == b"Z":
            break
    return rows, err


def main():
    shutil.rmtree(DATA_DIR, ignore_errors=True)
    if os.path.exists(VGLOG):
        os.remove(VGLOG)
    os.makedirs(DATA_DIR, exist_ok=True)
    # NOTE: --leak-check=full ONLY; never --errors-for-leaks=yes (exit(1) bug).
    proc = subprocess.Popen(
        [VALGRIND, "--tool=memcheck", "--leak-check=full",
         "--error-exitcode=99",
         "--log-file=" + VGLOG, BIN,
         "--data-dir", DATA_DIR, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    s = None
    for _ in range(150):
        try:
            s = connect()
            break
        except Exception:
            time.sleep(2)
    if s is None:
        print("VALGRIND: server never came up")
        proc.terminate()
        raise SystemExit(2)

    queries = [
        "create temp table sj (a int unique, b int, c int unique)",
        "insert into sj values (1, null, 2), (null, 2, null), (2, 1, 1)",
        "create temp table f1 (a int)",
        "create temp table f2 (a int)",
        "create temp table f3 (a int)",
        "create temp table f4 (a int)",
        # --- v1.72 3-way sj EC flip ---
        "explain (costs off) select * from sj t1, sj t2, sj t3 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a and t1.b = t3.b and t3.b = t3.a",
        # --- 4-way DP, EC no-op (written conjunct already canonical) ---
        "explain (costs off) select * from f1, f2, f3, f4 where f1.a = f2.a",
        # --- v1.71 2-way path regression ---
        "explain (costs off) select * from sj t1, sj t2 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a",
        # --- unaliased 3-way self-join: self-join guard fail-closed ---
        "explain (costs off) select * from sj, sj s2, sj s3 where sj.a = s2.a and s2.a = s3.a",
        # --- executor sanity (must be unaffected by EXPLAIN-text change) ---
        "select count(*) from sj t1, sj t2, sj t3 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a and t1.b = t3.b and t3.b = t3.a",
        "select count(*) from f1, f2, f3, f4 where f1.a = f2.a",
    ]
    for q in queries:
        rows, err = run_sql(s, q)
        if err:
            print("QUERY ERROR: %s -> %r" % (q[:60], err[:80]))
            raise SystemExit(3)
    # verify the v1.72 path actually fired (EC-canonical join filters)
    rows, _ = run_sql(s, "explain (costs off) select * from sj t1, sj t2, sj t3 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a and t1.b = t3.b and t3.b = t3.a")
    txt = b" ".join(rows).decode()
    assert "Join Filter: (t1.a = t3.b)" in txt and "Join Filter: (t1.a = t2.b)" in txt, txt
    print("v1.72 DP EC paths verified under memcheck")
    s.close()
    # give memcheck a clean shutdown so leaks are reported
    proc.terminate()
    rc = proc.wait(timeout=120)
    print("server exit code:", rc)

    log = open(VGLOG).read()
    m = re.search(r"ERROR SUMMARY: (\d+) errors", log)
    errs = int(m.group(1)) if m else -1
    print("ERROR SUMMARY:", errs)
    for kind in ("definitely lost", "possibly lost"):
        m = re.search(kind + r": ([\d,]+) bytes", log)
        print(kind, ":", m.group(1) if m else "?")
    if errs != 0:
        print("VALGRIND FAIL")
        raise SystemExit(1)
    print("VALGRIND CLEAN")


if __name__ == "__main__":
    main()
