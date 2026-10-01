#!/usr/bin/env python3
"""memcheck_v141.py — run the v1.41 feature paths under valgrind memcheck.

Exercises: pg_relation_size PG19 heap page accounting (fillfactor
variants, empty/small/large/toasted rows, multi-page), pg_attribute
attnum never-reuse (drop/add/drop/add), PARTITION OF attnum
inheritance, LIKE attnum compaction, WAL + checkpoint round-trip of
attnums/fillfactor.

Uses the DEBUG binary.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5541
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # pg_relation_size: heap page accounting
    "create table mc141a (a int, b text) with (fillfactor = 10)",
    "alter table mc141a alter column b set storage plain",
    "insert into mc141a (select 1, NULL)",
    "insert into mc141a (select 2, repeat('a', 1000))",
    "select pg_size_pretty(pg_relation_size('mc141a'::regclass, 'main'))",
    "select pg_relation_size('mc141a'::regclass)",
    "select pg_relation_size('mc141a')",
    # empty table -> 0
    "create table mc141empty (a int)",
    "select pg_relation_size('mc141empty')",
    # multi-page at default fillfactor
    "create table mc141b (a int, b text)",
    "insert into mc141b (select g, repeat('x', 100) from generate_series(1, 500) g)",
    "select pg_relation_size('mc141b')",
    # toasted values count as pointer datums
    "create table mc141t (a text)",
    "insert into mc141t values (repeat('z', 5000))",
    "select pg_relation_size('mc141t')",
    # attnum never-reuse
    "create table mc141c (a int, b int)",
    "alter table mc141c drop column a",
    "alter table mc141c add column a int",
    "alter table mc141c drop column a",
    "alter table mc141c add column a int not null",
    "select attname, attnum from pg_attribute where attrelid = 'mc141c'::regclass order by attname",
    # LIKE compacts attnums
    "create table mc141like (like mc141c)",
    "select attname, attnum from pg_attribute where attrelid = 'mc141like'::regclass order by attnum",
    # PARTITION OF inherits attnums
    "create table mc141p (a int, b int) partition by range (a)",
    "create table mc141p1 partition of mc141p for values from (1) to (10)",
    "alter table mc141p drop column a",
    "alter table mc141p add column a int",
    "select attrelid::regclass, attname, attnum from pg_attribute where attrelid in ('mc141p'::regclass, 'mc141p1'::regclass) order by 1, 2",
    # information_schema ordinal_position stays positional
    "select column_name, ordinal_position from information_schema.columns where table_name = 'mc141c' order by ordinal_position",
    "checkpoint",
    "drop table mc141a",
    "drop table mc141empty",
    "drop table mc141b",
    "drop table mc141t",
    "drop table mc141c",
    "drop table mc141like",
    "drop table mc141p1",
    "drop table mc141p",
    "checkpoint",
]

# SQL errors expected from the workload (code, count)
EXPECTED_SQL_ERRORS = {}


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc141_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not start under valgrind"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d

        def msg():
            t = rd(1)
            ln = struct.unpack("!i", rd(4))[0]
            return t, rd(ln - 4)

        while True:
            t, _ = msg()
            if t == b"Z":
                break

        def simple(q):
            qb = q.encode()
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            got_err = None
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            got_err = p[i + 1:e].decode()
                        i = e + 1
                if t == b"Z":
                    break
            return got_err

        from collections import Counter
        errs = Counter()
        for q in STMTS:
            err = simple(q)
            if err:
                errs[err] += 1
        print(f"SQL error counts: {dict(errs)}")
        if dict(errs) != EXPECTED_SQL_ERRORS:
            print(f"UNEXPECTED SQL error pattern (expected {EXPECTED_SQL_ERRORS})")
            sys.exit(3)
        print("SQL error pattern OK")
        s.close()
    finally:
        proc.terminate()
        try:
            rc = proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill(); rc = proc.wait(timeout=30)
        print(f"valgrind exit code: {rc}")
    # summarize the log
    import re
    txt = open(log).read()
    m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
    leaks = re.search(r"definitely lost: ([\d,]+) bytes", txt)
    ind = re.search(r"indirectly lost: ([\d,]+) bytes", txt)
    inv = len(re.findall(r"Invalid (read|write)", txt))
    print(f"valgrind ERROR SUMMARY: {m.group(1) if m else '?'} errors")
    print(f"definitely lost: {leaks.group(1) if leaks else '?'}; "
          f"indirectly lost: {ind.group(1) if ind else '?'}; "
          f"invalid accesses: {inv}")
    sys.exit(0 if (m and m.group(1) == "0") else 4)


if __name__ == "__main__":
    main()
