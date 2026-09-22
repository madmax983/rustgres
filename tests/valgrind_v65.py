#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.65 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises the
v0.65 DML target-semantics paths (serial backing sequences, nextval
defaults, INSERT first-N rule, DELETE aliases, DROP TABLE sequence
cleanup, ALTER ADD COLUMN serial), then shuts down and reports the
valgrind error summary. Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5566
DATA_DIR = "/tmp/rg65valgrind"
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = "/tmp/rg65valgrind.vglog"
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
        # serial DDL + sequence creation (all three pseudo-types)
        "CREATE TABLE vg_a (id SERIAL PRIMARY KEY, a INT, b TEXT)",
        "CREATE TABLE vg_b (s SMALLSERIAL, b BIGSERIAL, v INT DEFAULT 7)",
        "CREATE TEMP TABLE vg_t (id SERIAL, x INT)",
        # nextval defaults on insert (omitted + explicit + DEFAULT)
        "INSERT INTO vg_a (a) VALUES (10)",
        "INSERT INTO vg_a (a, b) VALUES (20, 'x'), (30, 'y')",
        "INSERT INTO vg_a VALUES (DEFAULT, 40, 'z')",
        "INSERT INTO vg_a VALUES (100, 50)",
        "INSERT INTO vg_b (v) VALUES (1), (2)",
        "INSERT INTO vg_t (x) VALUES (1)",
        # first-N rule: short rows, trailing defaults
        "CREATE TABLE vg_c (x INT, y INT, z TEXT DEFAULT 'dflt')",
        "INSERT INTO vg_c VALUES (1, 2), (3, 4)",
        "INSERT INTO vg_c (x) VALUES (5)",
        # error paths: count mismatches, ragged rows
        "INSERT INTO vg_c VALUES (1, 2, 3, 4)",
        "INSERT INTO vg_c (x, y) VALUES (1)",
        "INSERT INTO vg_c VALUES (1), (2, 3)",
        # DELETE aliases
        "DELETE FROM vg_a AS dt WHERE dt.a > 15",
        "DELETE FROM vg_a dt WHERE dt.a = 10",
        "DELETE FROM vg_b AS b2 WHERE b2.v = 1 RETURNING b2.s",
        # ALTER ADD COLUMN serial
        "ALTER TABLE vg_c ADD COLUMN ns SERIAL",
        "INSERT INTO vg_c (x) VALUES (9)",
        # sequence behavior: currval / setval / drop protection
        "SELECT nextval('vg_a_id_seq')",
        "SELECT currval('vg_a_id_seq')",
        "DROP SEQUENCE vg_a_id_seq",
        # DROP TABLE drops owned sequences
        "DROP TABLE vg_a",
        "SELECT nextval('vg_a_id_seq')",
        "DROP TABLE vg_b",
        "DROP TABLE vg_c",
        "DROP TABLE vg_t",
        # serial NULL/NOT NULL conflict
        "CREATE TABLE vg_d (id SERIAL NULL)",
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
