#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.49 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the
v0.49 parser paths (parenthesized first select item, quoted "char"
casts, scalar-subquery 21000), then shuts down and reports the
valgrind error summary. Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5560
DATA_DIR = "/tmp/rg50valgrind"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = "/tmp/rg50valgrind.vglog"


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
        # v0.49 new parse paths
        'SELECT (-1)::"char"',
        'SELECT (1+2)::int',
        "SELECT (-1)",
        "SELECT (SELECT 1)",
        'SELECT \'a\'::"char"::text',
        "SELECT CAST((-1) AS \"char\")",
        "CREATE TEMP TABLE vg_t (id integer)",
        "INSERT INTO vg_t VALUES (1), (2)",
        "SELECT (SELECT id FROM vg_t WHERE id = 1)",
        # multi-row scalar subquery -> 21000 through the new path
        "SELECT (SELECT id FROM vg_t)",
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
