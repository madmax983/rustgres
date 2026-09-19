#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.56 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the
v0.56 exact-decimal paths (storage::BigUint/BigDec behind
width_bucket, trunc_to_scale with extreme scales), then shuts down
and reports the valgrind error summary. Exit 0 iff: 0 ERROR SUMMARY
errors.
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5567
DATA_DIR = "/tmp/rg56valgrind"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = "/tmp/rg56valgrind.vglog"
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

    big = "1" + "0" * 300  # 10^300: exercises multi-limb BigUint paths
    queries = [
        # v0.56 exact-decimal width_bucket paths
        "SELECT width_bucket(0, -1e100::numeric, 1, 10)",
        "SELECT width_bucket(1, 1e100::numeric, 0, 10)",
        "SELECT width_bucket(0, -1e100::float8, 1, 10)",
        f"SELECT width_bucket(0, -{big}::numeric, {big}::numeric, 100)",
        f"SELECT width_bucket({big}::numeric, 0, {big}::numeric, 7)",
        "SELECT width_bucket(10.5::float8, -1.797e308::float8, 1.797e308::float8, 3)",
        "SELECT width_bucket(4.4925e307::float8, -8.985e307::float8, 8.985e307::float8, 10)",
        "SELECT width_bucket(0, 0, 5e-324, 4)",
        "SELECT width_bucket(4.5, 2, 8, 4)",
        "SELECT width_bucket(-5.2, 10, 0, 5)",
        "SELECT width_bucket('NaN', 3.0, 4.0, 888)",
        "SELECT width_bucket('Infinity'::float8, 1, 10, 10)",
        # v0.56 error paths (validation before the math)
        "SELECT width_bucket(5.0, 3.0, 4.0, 0)",
        "SELECT width_bucket(3.5, 3.0, 3.0, 888)",
        "SELECT width_bucket(0, 'NaN', 4.0, 888)",
        "SELECT width_bucket(2.0, 3.0, '-inf', 888)",
        "SELECT width_bucket(1::float8, 0, 1, 2147483647)",
        "SELECT width_bucket(NULL, 0, 10, 5)",
        "SELECT width_bucket(1, 0, NULL, 5)",
        # v0.56 two-argument trunc paths
        "SELECT trunc(19.99, 1)",
        "SELECT trunc(19.99, -1)",
        "SELECT trunc(1.99999, 3)",
        "SELECT trunc(1.5, -2147483648)",
        "SELECT trunc(1.5, 2147483647)",
        "SELECT trunc(9.9e131071, -131073)",
        "SELECT trunc(5e-16383, 1000000)",
        "SELECT trunc(19.99::float8, 1)",
        "SELECT trunc(1.5, NULL)",
        # width_bucket / trunc over table columns (describe + exec paths)
        "CREATE TEMP TABLE vg_wb (op numeric, b1 numeric, b2 numeric, c int)",
        f"INSERT INTO vg_wb VALUES (0, -{big}, {big}, 100), (4.5, 2, 8, 4), (NULL, 0, 10, 5)",
        "SELECT width_bucket(op, b1, b2, c) FROM vg_wb",
        "SELECT trunc(op, c) FROM vg_wb",
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
