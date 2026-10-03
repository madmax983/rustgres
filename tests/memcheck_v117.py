#!/usr/bin/env python3
"""memcheck_v117.py — valgrind memcheck for the v1.17 xmin/xmax paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; xmin/xmax DDL/DML, qualified
           and unqualified self-joins, the 42702 ambiguity path, user
           column shadowing, xid casts, xmax after DELETE.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           data persists; more traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5617

WORKLOAD = [
    ("ddl", "CREATE TABLE mc117t (a int, b text);", None),
    ("dml", "INSERT INTO mc117t VALUES (1, 'x'), (2, 'y'), (3, 'z');", None),
    ("q", "SELECT xmin, xmax FROM mc117t ORDER BY a;", None),
    # qualified self-join (the conformance statements)
    ("q", "SELECT a.xmin = b.xmin FROM mc117t a, mc117t b WHERE a.a=1 AND b.a=2;", None),
    ("q", "SELECT a.xmax = b.xmax FROM mc117t a, mc117t b WHERE a.a=1 AND b.a=3;", None),
    # qualified across different tables
    ("ddl", "CREATE TABLE mc117o (x int);", None),
    ("dml", "INSERT INTO mc117o VALUES (9);", None),
    ("q", "SELECT a.xmin, o.xmin FROM mc117t a, mc117o o WHERE a.a=1 AND o.x=9;", None),
    # unqualified ambiguity -> 42702
    ("q", "SELECT xmin FROM mc117t a, mc117t b LIMIT 1;", "42702"),
    # user column shadowing
    ("ddl", "CREATE TABLE mc117s (xmin int);", None),
    ("dml", "INSERT INTO mc117s VALUES (42);", None),
    ("q", "SELECT xmin FROM mc117s;", None),
    # xid casts
    ("q", "SELECT 123::xid;", None),
    ("q", "SELECT '456'::xid;", None),
    # xmax after delete
    ("dml", "DELETE FROM mc117t WHERE a = 1;", None),
    ("q", "SELECT a, xmax FROM mc117t ORDER BY a;", None),
    # xmin in WHERE / ORDER BY / GROUP BY
    ("q", "SELECT a FROM mc117t WHERE xmin > 0 ORDER BY xmin;", None),
    ("q", "SELECT xmin, count(*) FROM mc117t GROUP BY xmin;", None),
    ("ddl", "DROP TABLE mc117t;", None),
    ("ddl", "DROP TABLE mc117o;", None),
    ("ddl", "DROP TABLE mc117s;", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc117_",
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
