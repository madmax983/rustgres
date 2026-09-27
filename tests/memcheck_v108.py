#!/usr/bin/env python3
"""memcheck_v108.py — valgrind memcheck for the v1.08 EXPLAIN PG-text paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; EXPLAIN (COSTS OFF) traffic over
           every plan shape the v1.08 PG-text renderer touches:
           seq scans + filters, index scans (index cond + residual filter),
           inner/left joins (ON + WHERE splitting), sorts, limits,
           aggregates, CTEs, subqueries, VALUES, SIMILAR TO deparse,
           multi-option EXPLAIN parsing (duplicates last-wins, unknown
           option 42601, invalid boolean), COSTS ON legacy path,
           EXPLAIN ANALYZE (COSTS OFF) row-producing path.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           tables persist; more EXPLAIN traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5598

WORKLOAD = [
    # tables
    ("ddl", "CREATE TABLE mc108a(a int, b text, c numeric);", None),
    ("ddl", "CREATE TABLE mc108b(x int, y int);", None),
    ("ddl", "CREATE INDEX mc108ai ON mc108a(a);", None),
    ("dml", "INSERT INTO mc108a SELECT i, 'v'||i::text, i*1.5 FROM generate_series(1,200) g(i);", None),
    ("dml", "INSERT INTO mc108b SELECT i, i*10 FROM generate_series(1,100) g(i);", None),
    # COSTS OFF: every plan shape
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a WHERE a = 42;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a WHERE a > 10 AND a < 50;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a WHERE a = 42 AND b = 'v7';", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a WHERE b SIMILAR TO 'v[0-9]+';", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a a, mc108b b WHERE a.a = b.x;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a a, mc108b b WHERE a.a = b.x AND a.a > 5;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a LEFT JOIN mc108b ON a = x;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a ORDER BY a DESC, b;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM mc108a ORDER BY a + 1 LIMIT 5;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT count(*), sum(a) FROM mc108a GROUP BY b;", None),
    ("explain", "EXPLAIN (COSTS OFF) WITH w AS (SELECT * FROM mc108a WHERE a < 100) SELECT * FROM w;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM (SELECT a, b FROM mc108a WHERE a > 3) s WHERE s.a < 90;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (1),(2),(3)) v(n);", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT DISTINCT b FROM mc108a;", None),
    ("explain", "EXPLAIN (COSTS OFF) SELECT a FROM mc108a UNION SELECT x FROM mc108b;", None),
    # option forms
    ("explain", "EXPLAIN (COSTS OFF, VERBOSE) SELECT 1;", None),
    ("explain", "EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM mc108a WHERE a = 1;", None),
    ("explain", "EXPLAIN (COSTS ON, COSTS OFF) SELECT * FROM mc108a;", None),
    ("explain", "EXPLAIN (COSTS OFF, COSTS ON) SELECT * FROM mc108a;", None),
    ("explain", "EXPLAIN (BUFFERS, TIMING OFF, SUMMARY, SETTINGS, COSTS OFF) SELECT * FROM mc108a;", None),
    ("explain", "EXPLAIN (COSTS FALSE) SELECT * FROM mc108a;", None),
    ("explain", "EXPLAIN (COSTS 0) SELECT * FROM mc108a;", None),
    # error paths (parse-level; each Q is its own implicit block)
    ("explain", "EXPLAIN (FOOBAR) SELECT 1;", "42601"),
    ("explain", "EXPLAIN (COSTS frobnicate) SELECT 1;", "42601"),
    ("explain", "EXPLAIN (COSTS OFF) INSERT INTO mc108a VALUES (1,'x',2);", "42601"),
    # COSTS ON legacy path (unchanged behavior)
    ("explain", "EXPLAIN SELECT * FROM mc108a WHERE a = 1;", None),
    ("explain", "EXPLAIN (COSTS ON) SELECT * FROM mc108a a, mc108b b WHERE a.a = b.x;", None),
    # ANALYZE with COSTS OFF (row-producing v1.03 path + pg planning)
    ("explain", "EXPLAIN (ANALYZE, COSTS OFF) SELECT * FROM mc108a WHERE a < 50;", None),
    ("explain", "EXPLAIN (ANALYZE, COSTS OFF) SELECT count(*) FROM mc108a;", None),
    # txn-safety: EXPLAIN inside explicit txn must not abort it
    ("txn", "BEGIN; EXPLAIN (COSTS OFF) SELECT * FROM mc108a LEFT JOIN mc108b ON true; SELECT 1; COMMIT;", None),
    ("txn", "BEGIN; EXPLAIN (COSTS OFF) SELECT * FROM mc108a; ROLLBACK;", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc108_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log1 = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log1}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    nfail = [0]
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
        s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
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
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            codes = []
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            codes.append(p[i + 1:e].decode())
                        i = e + 1
                elif t == b"Z":
                    break
            return codes

        def expect_ok(name, codes, sql):
            if codes:
                nfail[0] += 1
                print(f"SQL ERR {codes}: {name}: {sql[:70]}")

        def expect_err(name, codes, want, sql):
            if codes != [want]:
                nfail[0] += 1
                print(f"SQL ERR {codes} (want [{want}]): {name}: {sql[:70]}")

        run_workload(simple, expect_ok, expect_err)
        expect_ok("checkpoint", simple("CHECKPOINT"), "CHECKPOINT")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    # Phase 2: restart under valgrind (WAL replay)
    log2 = os.path.join(data_dir, "vg2.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log2}",
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
            print("phase2: server did not start"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd2(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d

        def msg2():
            t = rd2(1)
            ln = struct.unpack("!i", rd2(4))[0]
            return t, rd2(ln - 4)

        while True:
            t, _ = msg2()
            if t == b"Z":
                break

        def simple2(q):
            qb = q.encode()
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            codes = []
            while True:
                t, p = msg2()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            codes.append(p[i + 1:e].decode())
                        i = e + 1
                elif t == b"Z":
                    break
            return codes

        if simple2("SELECT count(*) FROM mc108a;"):
            nfail[0] += 1; print("phase2: mc108a missing after replay")
        if simple2("EXPLAIN (COSTS OFF) SELECT * FROM mc108a WHERE a = 42 AND b = 'v7';"):
            nfail[0] += 1; print("phase2: explain failed after replay")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    print("SQLFAIL=" + str(nfail[0]))
    print("logs:", log1, log2)
    sys.exit(1 if nfail[0] else 0)


if __name__ == "__main__":
    main()
