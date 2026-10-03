#!/usr/bin/env python3
"""memcheck_v110.py — valgrind memcheck for the v1.10 LATERAL paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; LATERAL (SELECT ...) /
           LATERAL (VALUES ...) on comma/CROSS/INNER/LEFT/RIGHT joins,
           plus error paths (ragged VALUES, bad refs, 42601 join shape).
  phase 2: restart on the same datadir under valgrind (WAL replay);
           tables persist; more traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5603

WORKLOAD = [
    # LATERAL (SELECT ...) on comma join, correlated
    ("ddl", "CREATE TABLE mc110t (a int, b int);", None),
    ("ddl", "INSERT INTO mc110t VALUES (1, 10), (2, 20), (3, 30);", None),
    ("q", "SELECT t.a, s.y FROM mc110t t, LATERAL (SELECT t.a + t.b AS y) AS s;", None),
    # INNER JOIN LATERAL with ON
    ("q", "SELECT t.a, s.y FROM mc110t t INNER JOIN LATERAL (SELECT t.b * 2 AS y) AS s ON s.y > 30;", None),
    # LEFT JOIN LATERAL null-extension
    ("q", "SELECT t.a, s.y FROM mc110t t LEFT JOIN LATERAL (SELECT t.b AS y WHERE t.b > 100) AS s ON true;", None),
    # RIGHT/FULL JOIN LATERAL: correlated is 42P10 (PG19), uncorrelated ok
    ("q", "SELECT t.a, s.y FROM mc110t t RIGHT JOIN LATERAL (SELECT t.b AS y) AS s ON true;", "42P10"),
    ("q", "SELECT t.a, s.y FROM mc110t t FULL JOIN LATERAL (SELECT t.b AS y WHERE t.a = 2) AS s ON true;", "42P10"),
    ("q", "SELECT t.a, s.y FROM mc110t t RIGHT JOIN LATERAL (SELECT 99 AS y) AS s ON true;", None),
    ("q", "SELECT t.a, s.y FROM mc110t t FULL JOIN LATERAL (SELECT 99 AS y) AS s ON true;", None),
    # LATERAL (VALUES ...) correlated + uncorrelated
    ("q", "SELECT t.a, v.x FROM mc110t t, LATERAL (VALUES (t.a * 10), (t.b)) AS v(x);", None),
    ("q", "SELECT t.a, v.x FROM mc110t t, LATERAL (VALUES (1), (2)) AS v(x) WHERE t.a = 1;", None),
    # nested LATERAL (later sibling sees earlier)
    ("q", "SELECT t.a, s1.x, s2.y FROM mc110t t, LATERAL (SELECT t.a + 1 AS x) AS s1, LATERAL (SELECT s1.x + 1 AS y) AS s2 WHERE t.a = 2;", None),
    # multi-row correlated fan-out
    ("q", "SELECT t.a, s.z FROM mc110t t, LATERAL (SELECT t.a AS z UNION ALL SELECT t.b) AS s WHERE t.a = 1;", None),
    # scalar subquery inside LATERAL VALUES
    ("q", "SELECT t.a, v.x FROM mc110t t, LATERAL (VALUES ((SELECT t.a + 1))) AS v(x) WHERE t.a = 3;", None),
    # USING on a lateral join
    ("q", "SELECT a, y FROM mc110t t JOIN LATERAL (SELECT t.a AS a, t.b * 3 AS y) AS s USING (a);", None),
    # error paths
    ("q", "SELECT t.a, v.x FROM mc110t t, LATERAL (VALUES (1), (2, 3)) AS v(x);", "42601"),
    ("q", "SELECT t.a, s.y FROM mc110t t, LATERAL (SELECT nosuchcol AS y) AS s;", "42703"),
    ("q", "SELECT t.a, s.y FROM mc110t t, LATERAL ((SELECT 1) CROSS JOIN (SELECT 2)) AS s;", "42601"),
    ("q", "SELECT t.a, s.y FROM mc110t t, LATERAL (SELECT t.a AS y, t.a AS y2, nosuch AS y3) AS s;", "42703"),
    ("ddl", "DROP TABLE mc110t;", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc110_",
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
                print(f"UNEXPECTED ERR {name}: {codes} :: {sql[:60]}")
                nfail[0] += 1

        def expect_err(name, codes, want, sql):
            if codes != [want]:
                print(f"WRONG ERR {name}: got {codes} want [{want}] :: {sql[:60]}")
                nfail[0] += 1

        print("phase 1: fresh datadir", flush=True)
        run_workload(simple, expect_ok, expect_err)
        s.close()
        proc.terminate()
        proc.wait(timeout=120)

        # phase 2: WAL replay
        log2 = os.path.join(data_dir, "vg2.log")
        proc = subprocess.Popen(
            [VG, "--tool=memcheck", "--error-exitcode=99",
             "--errors-for-leak-kinds=none",
             f"--log-file={log2}",
             BIN, "--data-dir", data_dir, "--port", str(PORT)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not restart under valgrind"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = msg()
            if t == b"Z":
                break
        print("phase 2: WAL replay", flush=True)
        run_workload(simple, expect_ok, expect_err)
        # functions persisted across restart
        codes = simple("SELECT 1;")
        expect_ok("persisted table", codes, "SELECT 1")
        s.close()
        proc.terminate()
        rc = proc.wait(timeout=120)
        print(f"valgrind exit: {rc}, workload failures: {nfail[0]}")
        for log in (log1, log2):
            if os.path.exists(log):
                with open(log) as f:
                    txt = f.read()
                errs = txt.count("ERROR SUMMARY")
                print(f"{os.path.basename(log)}: {errs} summaries")
        sys.exit(1 if nfail[0] else 0)
    finally:
        try:
            proc.kill()
        except Exception:
            pass

if __name__ == "__main__":
    main()
