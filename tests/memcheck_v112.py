#!/usr/bin/env python3
"""memcheck_v112.py — valgrind memcheck for the v1.12 zero-target-list paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; SELECT;/SELECT FROM/zero-column
           UNION, INTERSECT, EXCEPT (incl. ALL), CTE variants, plus the
           error path (`SELECT FROM;` -> 42601).
  phase 2: restart on the same datadir under valgrind (WAL replay);
           tables persist; more traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5607

WORKLOAD = [
    ("ddl", "CREATE TABLE mc112t (a int, b text);", None),
    ("ddl", "INSERT INTO mc112t VALUES (1, 'x'), (2, 'y'), (3, 'z');", None),
    # bare / FROM-attached / WHERE-only empty selects
    ("q", "SELECT;", None),
    ("q", "SELECT FROM mc112t;", None),
    ("q", "SELECT FROM mc112t WHERE a > 1;", None),
    ("q", "SELECT WHERE false;", None),
    ("q", "SELECT FROM generate_series(1, 20);", None),
    ("q", "SELECT DISTINCT FROM generate_series(1, 20);", None),
    # zero-column set operations (all PG19 count variants)
    ("q", "SELECT UNION SELECT;", None),
    ("q", "SELECT INTERSECT SELECT;", None),
    ("q", "SELECT EXCEPT SELECT;", None),
    ("q", "SELECT FROM generate_series(1, 5) UNION SELECT FROM generate_series(1, 3);", None),
    ("q", "SELECT FROM generate_series(1, 5) UNION ALL SELECT FROM generate_series(1, 3);", None),
    ("q", "SELECT FROM generate_series(1, 5) INTERSECT SELECT FROM generate_series(1, 3);", None),
    ("q", "SELECT FROM generate_series(1, 5) INTERSECT ALL SELECT FROM generate_series(1, 3);", None),
    ("q", "SELECT FROM generate_series(1, 5) EXCEPT SELECT FROM generate_series(1, 3);", None),
    ("q", "SELECT FROM generate_series(1, 5) EXCEPT ALL SELECT FROM generate_series(1, 3);", None),
    # CTE variants
    ("q", "WITH cte AS MATERIALIZED (SELECT s FROM generate_series(1, 5) s) "
          "SELECT FROM cte UNION SELECT FROM cte;", None),
    ("q", "WITH cte AS NOT MATERIALIZED (SELECT s FROM generate_series(1, 5) s) "
          "SELECT FROM cte UNION SELECT FROM cte;", None),
    # zero-column subquery as derived table
    ("q", "SELECT count(*) FROM (SELECT FROM mc112t) z;", None),
    # error path: FROM with no table is still 42601
    ("q", "SELECT FROM;", "42601"),
    ("ddl", "DROP TABLE mc112t;", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc112_",
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

    print("memcheck_v112: %d failures" % nfail[0])
    return 1 if nfail[0] else 0


if __name__ == "__main__":
    sys.exit(main())
