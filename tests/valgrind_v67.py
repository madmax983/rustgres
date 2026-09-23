#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.67 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the
v0.67 numeric edge-case paths, then shuts down and reports the
valgrind error summary. Exit 0 iff: 0 ERROR SUMMARY errors.

New paths covered:
  * BigUint::div_rem_small / to_u32 (giant lcm Euclidean loop,
    from_big trailing-zero strip)
  * BigDec::div_exact digit-count short-circuit
  * gcd/lcm result conversion via Numeric::from_bigdec (131072-digit
    format limit instead of the i128 narrowing)
  * exp_var_inner PG19 guard (|x| >= 6000)
  * round(float8)/round(float4) -> float8 via rint (half-even)
  * 1e200 numeric plain-decimal output
  * smallint+smallint overflow (unchanged path, regression sweep)
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5567
DATA_DIR = "/tmp/rg67valgrind"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = "/tmp/rg67valgrind.vglog"
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
        # --- v0.67: giant lcm (div_rem_small + div_exact short-circuit)
        "SELECT lcm(9999 * (10::numeric)^131068 + ((10::numeric)^131068 - 1), 2)",
        # --- v0.67: gcd/lcm past i128, within the 131072-digit limit ---
        "SELECT gcd((10::numeric)^50, (10::numeric)^50)",
        "SELECT lcm((10::numeric)^50, 3::numeric)",
        "SELECT gcd(12, 8)",
        "SELECT lcm(12, 8)",
        "SELECT gcd(0, 0)",
        "SELECT lcm(0, 5)",
        # --- v0.67: exp PG19 guard (|x| >= 6000) -----------------------
        "SELECT exp(1000::numeric)",
        "SELECT exp(10000::numeric)",
        "SELECT exp(-10000::numeric)",
        "SELECT exp(1.0)",
        # --- v0.67: round(float8)/round(float4) -> float8 --------------
        "SELECT round('1.2345678901234e200'::float8)",
        "SELECT round('-1.2345678901234e200'::float8)",
        "SELECT round(2.5::float8)",
        "SELECT round(3.5::float8)",
        "SELECT round(2.5::float4)",
        "SELECT round(2.5::numeric)",
        "SELECT round(12345::numeric, -1)",
        # --- v0.67: 1e200 numeric plain-decimal output -----------------
        "SELECT '1e200'::numeric",
        "SELECT round('1e200'::numeric)",
        "SELECT trunc('1e200'::numeric)",
        "SELECT abs('-1e200'::numeric)",
        # --- smallint overflow (unchanged path, regression sweep) ------
        "SELECT 30000::smallint + 30000::smallint",
        "SELECT 100::smallint + 200::smallint",
        "SELECT 1",
    ]
    for i, q in enumerate(queries):
        print(f"  [{i+1}/{len(queries)}] {q[:70]}", flush=True)
        run_sql(s, q)
    s.close()
    # graceful shutdown so memcheck sees a clean exit
    proc.terminate()
    try:
        proc.wait(timeout=120)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    log = open(VGLOG).read() if os.path.exists(VGLOG) else ""
    m = re.search(r"ERROR SUMMARY: (\d+) errors", log)
    errs = int(m.group(1)) if m else -1
    print(f"VALGRIND ERROR SUMMARY: {errs} errors")
    if errs != 0:
        # print the interesting bits
        for line in log.splitlines():
            if "Invalid" in line or "definitely lost" in line or "ERROR SUMMARY" in line:
                print("  " + line[:200])
        raise SystemExit(1)
    print("VALGRIND: 0 errors — clean")


if __name__ == "__main__":
    main()
