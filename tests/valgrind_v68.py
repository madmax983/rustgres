#!/usr/bin/env python3
"""Valgrind memcheck driver for rustgres v0.68 new paths.

Starts the server under `valgrind --tool=memcheck`, exercises:
- GUC hardening: SET/RESET on PGC_INTERNAL GUCs (55P02), SHOW on them,
  unknown names still 42704.
- POSIX regex operators ~ / !~ / ~* / !~* (PG19 textregexeq family):
  match/non-match/case-insensitive, NULL propagation, 2201B on bad
  patterns, 42883 on non-text operands, bpchar coercion, WHERE scans,
  CHECK-constraint enforcement.

Exit 0 iff: 0 ERROR SUMMARY errors.
"""
import socket, struct, subprocess, time, os, sys, re

PORT = 5568
DATA_DIR = os.path.expanduser("~/workspace/rg68valgrind-data")
BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres"),
)
VGLOG = os.path.expanduser("~/workspace/rg68valgrind.vglog")
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
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    if os.path.exists(VGLOG):
        os.remove(VGLOG)
    os.makedirs(DATA_DIR, exist_ok=True)
    proc = subprocess.Popen(
        [VALGRIND, "--tool=memcheck", "--error-exitcode=99",
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
        # --- v0.68 GUC hardening ------------------------------------
        "SET server_version = 'bogus'",
        "SET server_version_num = 'bogus'",
        "RESET server_version",
        "RESET server_version_num",
        "SHOW server_version",
        "SHOW server_version_num",
        "SET nosuchguc = 'x'",
        "RESET nosuchguc",
        # --- v0.68 regex operators -----------------------------------
        "select 'abc' ~ '.*'",
        "select 'abc' !~ '.*'",
        "select 'ABC' ~ 'abc'",
        "select 'ABC' ~* 'abc'",
        "select 'ABC' !~* 'abc'",
        "select 'a1' ~ '[0-9]'",
        "select 'asdfghjkl;' ~ '.*asdf.*'",
        "select 'abc' ~ '^a.c$'",
        "select 'abc' ~ '^(a|b)c$'",
        "select NULL ~ 'x'",
        "select 'x' ~ NULL",
        "select 'ab' ~ '^a' = true",
        "select 'ab' ~ 'a' || 'b'",
        "select 'abc' ~ '('",            # 2201B
        "select 1 ~ 'x'",                # 42883
        "select 'x' !~* 1",              # 42883
        "select cast('abc' as bpchar) ~ 'b'",
        # regex over a table scan + check constraint
        "create table rgv (f1 text)",
        "insert into rgv values ('asdfghjkl;'), ('1234567890ABC'), ('343f%2a'), ('asdfghjkl;asdfghjkl;'), ('1234567890ABC1234567890ABC')",
        "select f1 from rgv c where c.f1 ~ '.*'",
        "select f1 from rgv c where c.f1 !~ '.*'",
        "select f1 from rgv c where c.f1 ~ '[0-9]'",
        "select f1 from rgv c where c.f1 ~ '.*asdf.*'",
        "create table rgvc (a text check (a ~* 'x'))",
        "insert into rgvc values ('xyz')",
        "insert into rgvc values ('abc')",  # 23514
        "drop table rgvc",
        "drop table rgv",
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
    print(f"valgrind_v68: ERROR SUMMARY: {errs} errors")
    if errs != 0:
        for line in log.splitlines():
            if re.match(r"==\d+== .* (at|by|Invalid|Leak|Uninitialised)", line):
                print(line)
        raise SystemExit(1)
    print("valgrind_v68: 0 errors — clean")


if __name__ == "__main__":
    main()
