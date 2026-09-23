#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.66 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the
v0.66 SET / SET LOCAL / SHOW / RESET / RESET ALL paths (all three
stateful GUCs, commit/abort/savepoint GUC-stack handling) and the
ALTER SEQUENCE RESTART fix, then shuts down and reports the valgrind
error summary. Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5566
DATA_DIR = "/tmp/rg66valgrind"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = "/tmp/rg66valgrind.vglog"
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
    s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
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
    subprocess.run(["rm", "-rf", DATA_DIR, VGLOG], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    subprocess.run(["pkill", "-9", "-x", "valgrind"], check=False)
    time.sleep(2)
    proc = subprocess.Popen(
        [VALGRIND, "--tool=memcheck", "--error-exitcode=99",
         "--log-file=" + VGLOG, BIN,
         "--data-dir", DATA_DIR, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    # valgrind startup is slow; wait for the port
    s = None
    for _ in range(120):
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
        # --- v0.66: SET / SHOW / RESET on every stateful GUC ---------
        "SHOW bytea_output",
        "SET bytea_output = 'escape'",
        "SHOW bytea_output",
        "SET bytea_output TO 'hex'",
        "SET default_transaction_read_only = 'on'",
        "SHOW default_transaction_read_only",
        "SET default_toast_compression = 'lz4'",
        "SHOW default_toast_compression",
        "SET SESSION bytea_output = 'escape'",
        # read-only SHOW values
        "SHOW server_version",
        "SHOW server_version_num",
        "SHOW transaction_isolation",
        # unknown GUC -> 42704, RESET unknown -> 42704
        "SET no_such_guc = 'x'",
        "RESET no_such_guc",
        "RESET bytea_output",
        "SHOW bytea_output",
        # --- SET LOCAL outside a transaction -> 25001 ---------------
        "SET LOCAL bytea_output = 'hex'",
        # --- SET LOCAL reverts at COMMIT -----------------------------
        "BEGIN",
        "SET LOCAL bytea_output = 'hex'",
        "SHOW bytea_output",
        "SET LOCAL default_toast_compression = 'pglz'",
        "SHOW default_toast_compression",
        "COMMIT",
        "SHOW bytea_output",
        "SHOW default_toast_compression",
        # --- SET LOCAL reverts at ROLLBACK ----------------------------
        "BEGIN",
        "SET LOCAL bytea_output = 'hex'",
        "SET LOCAL default_transaction_read_only = 'on'",
        "ROLLBACK",
        "SHOW bytea_output",
        "SHOW default_transaction_read_only",
        # --- SET then SET LOCAL: commit keeps SET --------------------
        "BEGIN",
        "SET bytea_output = 'hex'",
        "SET LOCAL bytea_output = 'escape'",
        "SHOW bytea_output",
        "COMMIT",
        "SHOW bytea_output",
        # --- SET LOCAL then SET: commit keeps the later SET ----------
        "BEGIN",
        "SET LOCAL bytea_output = 'hex'",
        "SET bytea_output = 'escape'",
        "SHOW bytea_output",
        "COMMIT",
        "SHOW bytea_output",
        # --- repeated SET LOCAL, latest wins -------------------------
        "BEGIN",
        "SET LOCAL bytea_output = 'hex'",
        "SET LOCAL bytea_output = 'escape'",
        "SHOW bytea_output",
        "ROLLBACK",
        "SHOW bytea_output",
        # --- plain SET in an aborted txn disappears ------------------
        "SET bytea_output = 'escape'",
        "BEGIN",
        "SET bytea_output = 'hex'",
        "ROLLBACK",
        "SHOW bytea_output",
        # --- savepoint rollback cancels later GUC changes ------------
        "BEGIN",
        "SET LOCAL bytea_output = 'hex'",
        "SAVEPOINT sp1",
        "SET LOCAL bytea_output = 'escape'",
        "SET default_toast_compression = 'lz4'",
        "SHOW bytea_output",
        "ROLLBACK TO sp1",
        "SHOW bytea_output",
        "SHOW default_toast_compression",
        "RELEASE sp1",
        "COMMIT",
        "SHOW bytea_output",
        # --- nested savepoints ---------------------------------------
        "BEGIN",
        "SAVEPOINT a",
        "SET LOCAL bytea_output = 'hex'",
        "SAVEPOINT b",
        "SET LOCAL bytea_output = 'escape'",
        "ROLLBACK TO b",
        "SHOW bytea_output",
        "ROLLBACK TO a",
        "SHOW bytea_output",
        "COMMIT",
        "SHOW bytea_output",
        # --- RESET in a txn reverts on abort --------------------------
        "SET bytea_output = 'escape'",
        "BEGIN",
        "RESET bytea_output",
        "SHOW bytea_output",
        "ROLLBACK",
        "SHOW bytea_output",
        # --- RESET ALL in a txn reverts on abort ----------------------
        "SET default_toast_compression = 'lz4'",
        "BEGIN",
        "RESET ALL",
        "SHOW default_toast_compression",
        "ROLLBACK",
        "SHOW default_toast_compression",
        "RESET ALL",
        "SHOW bytea_output",
        # --- v0.66: ALTER SEQUENCE RESTART = setval(r, false) ----------
        "CREATE SEQUENCE vg_seq START WITH 10 INCREMENT BY 5",
        "SELECT nextval('vg_seq')",
        "ALTER SEQUENCE vg_seq RESTART WITH 100",
        "SELECT nextval('vg_seq')",
        "SELECT nextval('vg_seq')",
        "ALTER SEQUENCE vg_seq RESTART",
        "SELECT nextval('vg_seq')",
        "DROP SEQUENCE vg_seq",
    ]
    for q in queries:
        try:
            run_sql(s, q)
        except Exception as e:
            print(f"query {q[:60]!r}: {e}")
    s.close()
    time.sleep(1)
    proc.terminate()
    try:
        proc.wait(timeout=120)
    except subprocess.TimeoutExpired:
        proc.kill()
        print("VALGRIND: did not exit after SIGTERM; killed")

    log = open(VGLOG, errors="replace").read()
    m = re.search(r"ERROR SUMMARY: (\d+) errors", log)
    errors = int(m.group(1)) if m else None
    print(f"valgrind memcheck: {errors} errors")
    if errors:
        for line in log.splitlines():
            if re.match(r"==\d+== (Invalid|Uninitialised|Mismatched|Syscall|Conditional)", line):
                print(line)
    raise SystemExit(0 if errors == 0 else 1)


if __name__ == "__main__":
    main()
