#!/usr/bin/env python3
"""memcheck_v116.py — valgrind memcheck for the v1.16 partition paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; RANGE/LIST/HASH partition DDL,
           routing inserts, multilevel routing, the v1.16 23514
           childless-intermediate error paths, ATTACH, parent scans.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           leaf data persists; more traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5613

WORKLOAD = [
    # RANGE
    ("ddl", "CREATE TABLE mc116r (a int, b int) PARTITION BY RANGE (a);", None),
    ("ddl", "CREATE TABLE mc116r1 PARTITION OF mc116r FOR VALUES FROM (0) TO (10);", None),
    ("ddl", "CREATE TABLE mc116r2 PARTITION OF mc116r FOR VALUES FROM (10) TO (20);", None),
    ("dml", "INSERT INTO mc116r VALUES (5, 50), (15, 150);", None),
    ("q", "SELECT * FROM mc116r ORDER BY a;", None),
    # LIST
    ("ddl", "CREATE TABLE mc116l (k text, v int) PARTITION BY LIST (k);", None),
    ("ddl", "CREATE TABLE mc116l1 PARTITION OF mc116l FOR VALUES IN ('a', 'b');", None),
    ("dml", "INSERT INTO mc116l VALUES ('a', 1);", None),
    ("q", "SELECT * FROM mc116l;", None),
    # HASH
    ("ddl", "CREATE TABLE mc116h (id int) PARTITION BY HASH (id);", None),
    ("ddl", "CREATE TABLE mc116h0 PARTITION OF mc116h FOR VALUES WITH (modulus 2, remainder 0);", None),
    ("ddl", "CREATE TABLE mc116h1 PARTITION OF mc116h FOR VALUES WITH (modulus 2, remainder 1);", None),
    ("dml", "INSERT INTO mc116h VALUES (7);", None),
    ("q", "SELECT count(*) FROM mc116h;", None),
    # multilevel + v1.16 23514 paths
    ("ddl", "CREATE TABLE mc116m (a int, b int, c text) PARTITION BY RANGE (a, b);", None),
    ("ddl", "CREATE TABLE mc116m5 PARTITION OF mc116m FOR VALUES FROM (1, 40) TO (1, 50) PARTITION BY RANGE (c);", None),
    ("ddl", "CREATE TABLE mc116m5c PARTITION OF mc116m5 FOR VALUES FROM ('c') TO ('d');", None),
    ("ddl", "CREATE TABLE mc116m5e PARTITION OF mc116m5 FOR VALUES FROM ('e') TO ('f') PARTITION BY LIST (c);", None),
    ("dml", "INSERT INTO mc116m VALUES (1, 45, 'c');", None),
    ("q", "SELECT * FROM mc116m5c;", None),
    ("q", "INSERT INTO mc116m VALUES (1, 45, 'e');", "23514"),
    ("q", "INSERT INTO mc116m VALUES (1, 45, 'z');", "23514"),
    ("q", "INSERT INTO mc116m VALUES (9, 99, 'q');", "23514"),
    # ATTACH
    ("ddl", "CREATE TABLE mc116a (a int, b text) PARTITION BY RANGE (a);", None),
    ("ddl", "CREATE TABLE mc116an (b text, a int);", None),
    ("ddl", "ALTER TABLE mc116a ATTACH PARTITION mc116an FOR VALUES FROM (100) TO (200);", None),
    ("dml", "INSERT INTO mc116a VALUES (150, 'x');", None),
    ("q", "SELECT a, b FROM mc116a;", None),
    # error paths: out-of-range, overlap
    ("q", "INSERT INTO mc116r VALUES (99, 99);", "23514"),
    ("ddl", "DROP TABLE mc116r;", None),
    ("ddl", "DROP TABLE mc116l;", None),
    ("ddl", "DROP TABLE mc116h;", None),
    ("ddl", "DROP TABLE mc116m;", None),
    ("ddl", "DROP TABLE mc116a;", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc116_",
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

        import re as _re
        def simple(sql):
            s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
            body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
            s.sendall(struct.pack("!i", len(body) + 4) + body)
            def rd(n):
                d = b""
                while len(d) < n:
                    c = s.recv(n - len(d))
                    if not c: raise RuntimeError("closed")
                    d += c
                return d
            def msg():
                t = rd(1); ln = struct.unpack("!i", rd(4))[0]
                return t, rd(ln - 4)
            while True:
                t, _ = msg()
                if t == b"Z": break
            s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1) + sql.encode() + b"\x00")
            codes = []
            while True:
                t, p = msg()
                if t == b"E":
                    m = _re.search(rb"C([0-9A-Z]{5})", p)
                    codes.append(m.group(1).decode() if m else "?????")
                elif t == b"Z":
                    break
            s.close()
            return codes

        def expect_ok(name, codes, sql):
            if codes:
                print(f"UNEXPECTED-ERR {name}: {codes} :: {sql[:80]}"); nfail[0] += 1
        def expect_err(name, codes, want, sql):
            if codes != [want]:
                print(f"WRONG-ERR {name}: got {codes} want [{want}] :: {sql[:80]}"); nfail[0] += 1

        run_workload(simple, expect_ok, expect_err)
        print(f"phase 1: {nfail[0]} workload failures")
        proc.terminate(); proc.wait(timeout=120)

        # phase 2: WAL replay on the same datadir
        log2 = os.path.join(data_dir, "vg2.log")
        proc2 = subprocess.Popen(
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
                print("server did not restart under valgrind"); proc2.kill(); sys.exit(2)
            run_workload(simple, expect_ok, expect_err)
            print(f"phase 2: {nfail[0]} workload failures (cumulative)")
        finally:
            proc2.terminate(); proc2.wait(timeout=120)

        # summarize valgrind errors
        for logf, phase in ((log1, 1), (log2, 2)):
            txt = open(logf).read() if os.path.exists(logf) else ""
            m = _re.search(r"ERROR SUMMARY: (\d+) errors", txt)
            errs = m.group(1) if m else "?"
            print(f"phase {phase} valgrind ERROR SUMMARY: {errs} errors")
            if errs != "0":
                for em in list(_re.finditer(r"==\d+== .*", txt))[:10]:
                    print("   ", em.group(0)[:160])
    finally:
        try:
            proc.terminate()
        except Exception:
            pass
    sys.exit(1 if nfail[0] else 0)


if __name__ == "__main__":
    main()
