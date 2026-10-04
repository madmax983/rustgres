#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v1.70 new paths.

Starts the server under `valgrind --tool=memcheck --leak-check=full`,
exercises the v1.70 contradictory-WHERE-qual paths:
- scan contradiction -> dummy Result (target 1)
- inner-join dummy-input propagation (target 2)
- lateral + const-false inner join propagation (third flip)
- fail-closed EXPLAIN probes (NULL, same literal, cross join, OR)
- executor sanity (plan paths must not affect execution)

Exit 0 iff: 0 ERROR SUMMARY errors and no definite/possible leaks.
"""
import socket, struct, subprocess, time, os, sys, re, shutil

PORT = 5570
DATA_DIR = os.path.expanduser("~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v170/vg170-data")
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = os.path.expanduser("~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v170/valgrind_v170.log")
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
        "create temp table parent (k int primary key, pd int)",
        "create temp table child (k int unique, cd int)",
        "insert into parent values (1, 10), (2, 20), (3, 30)",
        "insert into child values (1, 100), (4, 400)",
        "create temp table t(a int)",
        "insert into t values (1), (2)",
        # --- v1.70 scan contradiction (target 1) ---
        "explain (costs off) select p.* from parent p left join child c on (p.k = c.k) where p.k = 1 and p.k = 2",
        # --- v1.70 join propagation (target 2) ---
        "explain (costs off) select p.* from (parent p left join child c on (p.k = c.k)) join parent x on p.k = x.k where p.k = 1 and p.k = 2",
        # --- v1.70 third flip: lateral + const-false inner join ---
        "explain (costs off) select 1 from t t1 join lateral (select t1.a from (select 1) foo offset 0) as s1 on true join (select 1 from t t2 inner join (t t3 left join (t t4 left join t t5 on t4.a = 1) on t3.a = t4.a) on false where t3.a = coalesce(t5.a,1)) as s2 on true",
        # --- fail-closed probes ---
        "explain (costs off) select p.* from parent p where p.k = 1 and p.k = null",
        "explain (costs off) select p.* from parent p where p.k = 1 and p.k = 1",
        "explain (costs off) select p.* from parent p where p.k = 1 and p.k > 2",
        "explain (costs off) select p.* from parent p, parent x where p.k = x.k and p.k = 1",
        "explain (costs off) select p.* from parent p where p.k = 1 and p.k = 2 or p.k = 3",
        # --- executor sanity (must be unaffected) ---
        "select p.* from parent p left join child c on (p.k = c.k) where p.k = 1 and p.k = 2",
        "select p.* from parent p where p.k = 1 and p.k = 1",
        "select count(*) from parent",
        "select 1 from t t1 join t t2 on t1.a = t2.a",
    ]
    for q in queries:
        rows, err = run_sql(s, q)
        if err:
            print("QUERY ERROR: %s -> %r" % (q[:60], err[:80]))
            raise SystemExit(3)
    # verify the v1.70 paths actually fired (plan text contains the markers)
    rows, _ = run_sql(s, "explain (costs off) select p.* from parent p left join child c on (p.k = c.k) where p.k = 1 and p.k = 2")
    txt = b" ".join(rows).decode()
    assert "Replaces: Scan on p" in txt and "One-Time Filter: false" in txt, txt
    rows, _ = run_sql(s, "explain (costs off) select p.* from (parent p left join child c on (p.k = c.k)) join parent x on p.k = x.k where p.k = 1 and p.k = 2")
    txt = b" ".join(rows).decode()
    assert "Replaces: Join on p, x" in txt and "One-Time Filter: false" in txt, txt
    print("v1.70 paths verified under memcheck")
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
