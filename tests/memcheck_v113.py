#!/usr/bin/env python3
"""memcheck_v113.py — valgrind memcheck for the v1.13 tableoid paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; partitioned table, tableoid /
           tableoid::regclass in SELECT / WHERE / GROUP BY / ORDER BY,
           pg_size_pretty, plain-table tableoid.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           tables persist; more traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5608

WORKLOAD = [
    ("ddl", "CREATE TABLE mc113t (a text, b int) PARTITION BY LIST (a);", None),
    ("ddl", "CREATE TABLE mc113p1 PARTITION OF mc113t FOR VALUES IN ('x');", None),
    ("ddl", "CREATE TABLE mc113p2 PARTITION OF mc113t FOR VALUES IN ('y');", None),
    ("ddl", "INSERT INTO mc113t VALUES ('x', 1), ('y', 2), ('x', 3);", None),
    # tableoid system column: leaf OID and ::regclass rendering
    ("q", "SELECT tableoid::regclass, a, b FROM mc113t ORDER BY a, b;", None),
    ("q", "SELECT tableoid FROM mc113t;", None),
    ("q", "SELECT tableoid::regclass::text, count(*) FROM mc113t GROUP BY 1 ORDER BY 1;", None),
    ("q", "SELECT a FROM mc113t WHERE tableoid::regclass = 'mc113p1';", None),
    ("q", "SELECT tableoid::regclass AS p, b FROM mc113t ORDER BY p, b;", None),
    # pg_size_pretty formatting
    ("q", "SELECT pg_size_pretty(8192);", None),
    ("q", "SELECT pg_size_pretty(10485760);", None),
    ("q", "SELECT pg_size_pretty(pg_relation_size('mc113t'::regclass));", None),
    # plain table tableoid
    ("ddl", "CREATE TABLE mc113plain (a int);", None),
    ("ddl", "INSERT INTO mc113plain VALUES (1), (2);", None),
    ("q", "SELECT tableoid::regclass FROM mc113plain;", None),
    ("ddl", "DROP TABLE mc113plain;", None),
    ("ddl", "DROP TABLE mc113t;", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc113_",
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
                print("phase 2 server did not start"); proc2.kill(); sys.exit(2)
            run_workload(simple, expect_ok, expect_err)
            print(f"phase 2: {nfail[0]} workload failures (cumulative)")
        finally:
            proc2.terminate(); proc2.wait(timeout=120)

        # summarize valgrind errors
        for log, phase in [(log1, "phase1"), (log2, "phase2")]:
            try:
                with open(log) as f:
                    txt = f.read()
                import re as _re2
                m = _re2.search(r"ERROR SUMMARY: (\d+) errors", txt)
                ctx = _re2.search(r"(\d+) contexts", txt)
                print(f"{phase}: {m.group(0) if m else 'no summary'}"
                      f"{', ' + ctx.group(0) if ctx else ''}")
                if m and m.group(1) != "0":
                    nfail[0] += 1
            except FileNotFoundError:
                print(f"{phase}: no log"); nfail[0] += 1
    finally:
        try: proc.kill()
        except Exception: pass

    print("memcheck_v113: %d failures" % nfail[0])
    return 1 if nfail[0] else 0


if __name__ == "__main__":
    sys.exit(main())
