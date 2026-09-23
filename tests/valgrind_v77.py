#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.77 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises:
- Temp-table ALTER: ADD/DROP/RENAME COLUMN, ADD CONSTRAINT NOT NULL
  NOT VALID + UNIQUE, RENAME TO, DROP CONSTRAINT, plus the
  temp-shadows-permanent isolation (global index catalog untouched).
- CheckKind classification: user CHECK (col IS NOT NULL) -> 23514,
  ALTER-added NOT NULL -> 23502.
- Simple-protocol parse error failing an explicit transaction
  (25P02 until ROLLBACK).
- UPDATE...FROM / DELETE...USING ambiguous columns (42702).
- Quantified comparisons over empty sets.

Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re, shutil

PORT = 5569
DATA_DIR = os.path.expanduser("~/workspace/rg77valgrind-data")
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = os.path.expanduser("~/workspace/rg77valgrind.vglog")
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

    queries = [
        # --- v0.77 temp ALTER --------------------------------------
        "CREATE TEMP TABLE v77t (id int, v int)",
        "INSERT INTO v77t VALUES (1, 10), (2, NULL), (NULL, 30)",
        "ALTER TABLE v77t ADD CONSTRAINT nn NOT NULL v NOT VALID",
        "SELECT count(*) FROM v77t",
        "INSERT INTO v77t VALUES (3, NULL)",          # 23502
        "ALTER TABLE v77t ADD CONSTRAINT uq UNIQUE (id)",
        "INSERT INTO v77t VALUES (1, 40)",            # 23505
        "INSERT INTO v77t VALUES (NULL, 41)",
        "INSERT INTO v77t VALUES (NULL, 42)",         # NULLs distinct
        "ALTER TABLE v77t ADD COLUMN w int DEFAULT 7",
        "SELECT w FROM v77t WHERE id = 1",
        "ALTER TABLE v77t ADD COLUMN x int NOT NULL", # 23502: existing rows
        "ALTER TABLE v77t DROP COLUMN w",
        "ALTER TABLE v77t RENAME COLUMN v TO vv",
        "SELECT vv FROM v77t ORDER BY id",
        "ALTER TABLE v77t RENAME TO v77t2",
        "SELECT vv FROM v77t2 ORDER BY id",
        "ALTER TABLE v77t2 DROP CONSTRAINT nn",
        "INSERT INTO v77t2 VALUES (4, NULL)",
        # --- v0.77 temp-shadows-permanent isolation -----------------
        "CREATE TABLE v77s (id int, v int)",
        "CREATE INDEX v77s_idx ON v77s(v)",
        "INSERT INTO v77s VALUES (1, 1)",
        "CREATE TEMP TABLE v77s (id int, v int)",
        "INSERT INTO v77s VALUES (2, 2)",
        "ALTER TABLE v77s RENAME COLUMN v TO w",
        "ALTER TABLE v77s DROP COLUMN w CASCADE",
        "ALTER TABLE v77s ADD CONSTRAINT uq2 UNIQUE (id)",
        "DROP TABLE v77s",                            # drops the temp one
        "SELECT v FROM v77s",                         # permanent table intact
        # --- v0.77 CheckKind: 23514 vs 23502 ------------------------
        "CREATE TABLE v77c (id int)",
        "ALTER TABLE v77c ADD CONSTRAINT ck CHECK (id IS NOT NULL)",
        "INSERT INTO v77c VALUES (NULL)",             # 23514
        "CREATE TABLE v77n (id int)",
        "ALTER TABLE v77n ADD CONSTRAINT nn2 NOT NULL id",
        "INSERT INTO v77n VALUES (NULL)",             # 23502
        "DROP TABLE v77c",
        "DROP TABLE v77n",
        # --- v0.77 parse error fails explicit txn -------------------
        "BEGIN",
        "SELEC oops FROM nowhere",                    # 42601, txn failed
        "SELECT 1",                                   # 25P02
        "SELECT 2",                                   # 25P02
        "ROLLBACK",
        "SELECT 42",
        # --- v0.77 ambiguity / quantified ---------------------------
        "CREATE TABLE v77a (id int, v int)",
        "CREATE TABLE v77b (id int, v int)",
        "INSERT INTO v77a VALUES (1, 10)",
        "INSERT INTO v77b VALUES (1, 20)",
        "UPDATE v77a SET v = 0 FROM v77b WHERE v = 1", # 42702
        "UPDATE v77a SET v = v77b.v FROM v77b WHERE v77a.id = v77b.id",
        "DELETE FROM v77a USING v77b WHERE v77a.id = v77b.id",
        "SELECT 1 = ANY (SELECT id FROM v77a WHERE id > 99)",
        "SELECT 1 <> ALL (SELECT id FROM v77a WHERE id > 99)",
        "DROP TABLE v77a",
        "DROP TABLE v77b",
        "DROP TABLE v77s",
    ]
    for q in queries:
        try:
            run_sql(s, q)
        except Exception as e:
            print(f"query failed ({q}): {e}")
    s.close()
    # graceful shutdown so valgrind sees a clean exit
    try:
        proc.terminate()
        proc.wait(timeout=120)
    except Exception:
        proc.kill()
    log = open(VGLOG).read() if os.path.exists(VGLOG) else ""
    m = re.search(r"ERROR SUMMARY: (\d+) errors", log)
    errs = int(m.group(1)) if m else -1
    print(f"valgrind_v77: ERROR SUMMARY: {errs} errors")
    if errs != 0:
        # show the first error blocks for triage
        blocks = re.findall(r"(==\d+== .*?(?:\n==\d+== |$))", log, re.S)
        for b in blocks[:6]:
            print(b[:1200])
        raise SystemExit(1)
    print("VALGRIND: 0 errors — clean")


if __name__ == "__main__":
    main()
