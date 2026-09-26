#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v1.05 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the v1.05
TOAST UPDATE lifecycle paths:
- UPDATE with unchanged toasted column (value-id reuse / TOASTCOL_IGNORE)
- UPDATE with changed toasted column (eager chunk delete)
- Toasted -> inline UPDATE (old chunks deleted transactionally)
- ROLLBACK of UPDATE (chunks + metadata restored/pruned)
- INSERT .. ON CONFLICT DO UPDATE (cleanup + reuse)
- Partition-moving UPDATE (chunk cleanup on source leaf)
- Explicit-transaction UPDATE (visibility of eager cleanup)

Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re, shutil, random

PORT = 5570
DATA_DIR = os.path.expanduser("~/workspace/rg105valgrind-data")
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = os.path.expanduser("~/workspace/rg105valgrind.vglog")
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
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        read_exact(s, ln - 4)
        if t == b"Z":
            break


def main():
    shutil.rmtree(DATA_DIR, ignore_errors=True)
    if os.path.exists(VGLOG):
        os.remove(VGLOG)
    os.makedirs(DATA_DIR, exist_ok=True)
    proc = subprocess.Popen(
        [VALGRIND, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=definite,possible",
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

    random.seed(105)
    bigr = "".join(random.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(5000))
    bigr2 = "".join(random.choice("ABCDEFGHIJKLMNOPQRSTUVWXYZ9876543210") for _ in range(5000))

    queries = [
        # --- v1.05 UPDATE reuse (unchanged toasted column) -----------
        "CREATE TABLE v105u(a int, b text)",
        f"INSERT INTO v105u VALUES (1, '{bigr}')",
        "BEGIN",
        f"UPDATE v105u SET b = '{bigr}' WHERE a = 1",   # unchanged: reuse vid
        "COMMIT",
        f"SELECT b FROM v105u WHERE a = 1",
        # --- v1.05 UPDATE changed toasted column (eager delete) -------
        "BEGIN",
        f"UPDATE v105u SET b = '{bigr2}' WHERE a = 1",
        "COMMIT",
        f"SELECT b FROM v105u WHERE a = 1",
        # --- v1.05 toasted -> inline ---------------------------------
        "UPDATE v105u SET b = 'tiny' WHERE a = 1",
        "SELECT b FROM v105u WHERE a = 1",
        f"UPDATE v105u SET b = '{bigr}' WHERE a = 1",
        # --- v1.05 ROLLBACK of UPDATE --------------------------------
        "BEGIN",
        f"UPDATE v105u SET b = '{bigr2}' WHERE a = 1",
        "ROLLBACK",
        f"SELECT b FROM v105u WHERE a = 1",
        "BEGIN",
        f"INSERT INTO v105u VALUES (2, '{bigr2}')",
        "ROLLBACK",
        "SELECT count(*) FROM v105u",
        # --- v1.05 upsert --------------------------------------------
        "CREATE TABLE v105c(a int PRIMARY KEY, b text)",
        f"INSERT INTO v105c VALUES (1, '{bigr}')",
        f"INSERT INTO v105c VALUES (1, '{bigr2}') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
        f"INSERT INTO v105c VALUES (1, '{bigr2}') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
        f"SELECT b FROM v105c WHERE a = 1",
        # --- v1.05 partition-moving UPDATE ----------------------------
        "CREATE TABLE v105p(a int, b text) PARTITION BY LIST (a)",
        "CREATE TABLE v105p1 PARTITION OF v105p FOR VALUES IN (1)",
        "CREATE TABLE v105p2 PARTITION OF v105p FOR VALUES IN (2)",
        f"INSERT INTO v105p VALUES (1, '{bigr}')",
        "UPDATE v105p SET a = 2 WHERE a = 1",
        f"SELECT b FROM v105p WHERE a = 2",
        # --- v1.05 compressed toast reuse -----------------------------
        "CREATE TABLE v105x(a int, b text)",
        f"INSERT INTO v105x VALUES (1, '{'x' * 5000}')",  # pglz inline
        f"UPDATE v105x SET b = '{'x' * 5000}' WHERE a = 1",
        f"SELECT b FROM v105x WHERE a = 1",
        "DROP TABLE v105x",
        "DROP TABLE v105p",
        "DROP TABLE v105c",
        "DROP TABLE v105u",
    ]
    for q in queries:
        try:
            run_sql(s, q)
        except Exception as e:
            print(f"query failed ({q[:60]}): {e}")
    s.close()
    # graceful shutdown so valgrind sees a clean exit
    try:
        proc.terminate()
        proc.wait(timeout=180)
    except Exception:
        proc.kill()
    log = open(VGLOG).read() if os.path.exists(VGLOG) else ""
    m = re.search(r"ERROR SUMMARY: (\d+) errors", log)
    errs = int(m.group(1)) if m else -1
    print(f"valgrind_v105: ERROR SUMMARY: {errs} errors")
    if errs != 0:
        blocks = re.findall(r"(==\d+== .*?(?:\n==\d+== |$))", log, re.S)
        for b in blocks[:6]:
            print(b[:1200])
        raise SystemExit(1)
    print("VALGRIND: 0 errors — clean")


if __name__ == "__main__":
    main()
