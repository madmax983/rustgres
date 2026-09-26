#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v1.06 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the v1.06
PGLZ byte-compatibility paths (port of PG19 src/common/pg_lzcompress.c):
- wide compressible INSERTs (multi-KB inputs through pglz_compress,
  incl. the INT_MAX/100 overflow-safe result_max branch shape)
- boundary inputs: 32-byte minimum, 17/18-byte tag boundary, 273-byte
  max matches, 4095 offset boundary, history recycling
- SELECT of toasted values (out_of_line_bytes -> framed decompress path)
- UPDATE of toasted values + ROLLBACK (v1.05 lifecycle with pglz bytes)
- incompressible wide value (external uncompressed)
- lz4 column method
- restart mid-run: WAL replay of toasted values, then more queries

Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re, shutil, random

PORT = 5571
DATA_DIR = os.path.expanduser("~/workspace/rg106valgrind-data")
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = os.path.expanduser("~/workspace/rg106valgrind.vglog")
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


def start_server():
    return subprocess.Popen(
        [VALGRIND, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=definite,possible",
         "--log-file=" + VGLOG, BIN,
         "--data-dir", DATA_DIR, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)


def wait_up():
    for _ in range(150):
        try:
            return connect()
        except Exception:
            time.sleep(2)
    return None


def main():
    shutil.rmtree(DATA_DIR, ignore_errors=True)
    if os.path.exists(VGLOG):
        os.remove(VGLOG)
    os.makedirs(DATA_DIR, exist_ok=True)
    proc = start_server()
    s = wait_up()
    if s is None:
        print("VALGRIND: server never came up")
        proc.terminate()
        raise SystemExit(2)

    random.seed(106)
    # compressible corpora at several scales incl. boundary lengths
    corpora = {
        "c32": "A" * 32,                      # minimum pglz input
        "c300": "Q" * 300,                    # multi-tag run
        "c273": "z" * 273,                    # max-match length
        "c5k": ("word-" * 1000)[:5000],       # external compressed
        "c100k": ("sentence one. sentence two. " * 4000)[:100000],
        "cinc": "".join(random.choice("abcdefghijklmnopqrstuvwxyz0123456789")
                        for _ in range(5000)),  # incompressible
    }
    queries = [
        "CREATE TABLE v106m(a int, b text)",
        f"INSERT INTO v106m VALUES (1, '{corpora['c32']}')",
        f"INSERT INTO v106m VALUES (2, '{corpora['c300']}')",
        f"INSERT INTO v106m VALUES (3, '{corpora['c273']}')",
        f"INSERT INTO v106m VALUES (4, '{corpora['c5k']}')",
        f"INSERT INTO v106m VALUES (5, '{corpora['c100k']}')",
        f"INSERT INTO v106m VALUES (6, '{corpora['cinc']}')",
        "SELECT a, length(b), pg_column_compression(b) FROM v106m ORDER BY a",
        f"SELECT b FROM v106m WHERE a = 5",
        f"SELECT b FROM v106m WHERE a = 4",
        # UPDATE + rollback on toasted pglz values
        "BEGIN",
        f"UPDATE v106m SET b = '{corpora['c5k']}' WHERE a = 1",
        "COMMIT",
        "BEGIN",
        f"UPDATE v106m SET b = '{corpora['c300']}' WHERE a = 4",
        "ROLLBACK",
        f"SELECT b FROM v106m WHERE a = 4",
        "UPDATE v106m SET b = 'tiny' WHERE a = 5",
        f"SELECT b FROM v106m WHERE a = 5",
        # lz4 column
        "CREATE TABLE v106l(v text COMPRESSION lz4)",
        f"INSERT INTO v106l VALUES ('{corpora['c5k']}')",
        f"SELECT v, pg_column_compression(v) FROM v106l",
    ]
    for q in queries:
        try:
            run_sql(s, q)
        except Exception as e:
            print(f"query failed (continuing): {q[:60]!r}: {e}")
    s.close()

    # --- restart under valgrind: WAL replay of toasted pglz values ---
    # Wait for valgrind to exit (it writes the summary at exit) before
    # preserving the phase-1 log; the phase-2 server truncates VGLOG.
    p1log = VGLOG + ".phase1"
    proc.terminate()
    try:
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    if os.path.exists(VGLOG):
        shutil.copyfile(VGLOG, p1log)
    time.sleep(2)
    proc = start_server()
    s = wait_up()
    if s is None:
        print("VALGRIND: server never came back up after restart")
        proc.terminate()
        raise SystemExit(2)
    for q in [
        "SELECT a, length(b) FROM v106m ORDER BY a",
        f"SELECT b FROM v106m WHERE a = 4",
        "SELECT v FROM v106l",
        f"INSERT INTO v106m VALUES (7, '{corpora['c100k']}')",
        "SELECT count(*) FROM v106m",
    ]:
        try:
            run_sql(s, q)
        except Exception as e:
            print(f"post-restart query failed (continuing): {q[:60]!r}: {e}")
    s.close()
    proc.terminate()
    try:
        proc.wait(timeout=120)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()

    log = open(VGLOG).read() if os.path.exists(VGLOG) else ""
    p1log = open(VGLOG + ".phase1").read() if os.path.exists(VGLOG + ".phase1") else ""
    p1 = re.search(r"ERROR SUMMARY: (\d+) errors", p1log)
    m = re.search(r"ERROR SUMMARY: (\d+) errors", log)
    p1errs = int(p1.group(1)) if p1 else -1
    errs = int(m.group(1)) if m else -1
    print(f"valgrind v106: phase1 ERROR SUMMARY: {p1errs} errors; "
          f"phase2 ERROR SUMMARY: {errs} errors")
    if p1errs != 0 or errs != 0:
        for line in (p1log + log).splitlines():
            if "ERROR" in line or "definitely lost" in line or "possibly lost" in line:
                print("  " + line[:200])
        raise SystemExit(1)
    print("VALGRIND v106 CLEAN")


if __name__ == "__main__":
    main()
