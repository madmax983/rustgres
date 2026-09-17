#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.51 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the
v0.49 parser paths (parenthesized first select item, quoted "char"
casts, scalar-subquery 21000), then shuts down and reports the
valgrind error summary. Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5561
DATA_DIR = "/tmp/rg51valgrind"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = "/tmp/rg51valgrind.vglog"


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
        ["valgrind", "--tool=memcheck", "--error-exitcode=99",
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
        # v0.51 new bitwise paths (int2 returns int2, wrap-then-truncate shifts)
        "SELECT 12::int2 & 10::int2",
        "SELECT 12::int2 | 10::int2",
        "SELECT 12::int2 # 10::int2",
        "SELECT 1::int2 << 15::int2",
        "SELECT 20000::int2 << 2::int2",
        "SELECT (-1)::int2 >> 1::int2",
        "SELECT (-32768)::int2 >> 15::int2",
        "SELECT NULL::int2 & 1::int2",
        "SELECT NULL::int2 << 3::int2",
        "SELECT 12::int4 & 10::int4",
        "SELECT 1::int4 << 31::int4",
        "SELECT (-1)::int4 >> 1::int4",
        "SELECT 12::int8 & 10::int8",
        "SELECT 1::int8 << 63::int8",
        "SELECT ~(5::int2)",
        "SELECT ~(5::int8)",
        "SELECT 5::int2 | 2::int4",
        "SELECT 30000::int2 & 30000::int2",
        # int2 bitwise through a table column (describe + exec paths)
        "CREATE TEMP TABLE vg_i2 (a smallint)",
        "INSERT INTO vg_i2 VALUES (1), (20000), (-1), (NULL)",
        "SELECT a & 10::int2, a | 10::int2, a << 2::int2, a >> 1::int2 FROM vg_i2",
    ]
    for q in queries:
        try:
            run_sql(s, q)
        except Exception as e:
            print(f"query {q!r}: {e}")
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
    print("VALGRIND ERROR SUMMARY:", m.group(0) if m else "not found")
    for pat in ["definitely lost", "indirectly lost", "possibly lost",
                "Invalid read", "Invalid write"]:
        mm = re.findall(rf"{pat}[^\n]*", log)
        seen = set()
        for line in mm:
            line = line.strip()
            if line not in seen:
                seen.add(line)
                print("  " + line)
                if len(seen) > 6:
                    print("  ... (truncated)")
                    break
    if errors is None:
        print("VALGRIND: could not parse error summary")
        raise SystemExit(2)
    raise SystemExit(0 if errors == 0 else 1)


if __name__ == "__main__":
    main()
