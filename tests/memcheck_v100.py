#!/usr/bin/env python3
"""memcheck_v100.py — run the v1.00 partition + trigger paths under valgrind memcheck.

Exercises:
  A. Partition DDL: RANGE/LIST/HASH PARTITION BY, PARTITION OF, ATTACH.
  B. Partition routing: inserts routed to correct leaves, parent SELECT.
  C. BEFORE INSERT triggers during routing (leaf trigger firing).
  D. Error paths: overlap (23514), no partition (23514), leaf violation (23514).
  E. pg_class.relkind queries.
  F. Restart: checkpoint, shutdown, restart, verify.

Two-phase: phase 1 runs the workload, checkpoints, and shuts down cleanly;
phase 2 restarts the server on the same datadir (WAL replay) and verifies.
Valgrind runs with --errors-for-leak-kinds=none (no leak checking, only
memory errors); exit code 99 on any memcheck error.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5549
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc100_")
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
                elif t == b"Z":
                    break
            return got_err

        nfail = 0
        def expect_ok(name, err):
            nonlocal nfail
            if err:
                nfail += 1
                print(f"SQL ERR {err}: {name[:70]}")

        def expect_err(name, err, want):
            nonlocal nfail
            if err != want:
                nfail += 1
                print(f"SQL ERR {err} (want {want}): {name[:70]}")

        # ---- A/B: partition DDL + routing ----
        expect_ok("create range", simple("CREATE TABLE mcp (a INT, b TEXT) PARTITION BY RANGE (a)"))
        expect_ok("partition of", simple("CREATE TABLE mcp1 PARTITION OF mcp FOR VALUES FROM (1) TO (100)"))
        expect_ok("partition of 2", simple("CREATE TABLE mcp2 PARTITION OF mcp FOR VALUES FROM (100) TO (200)"))
        expect_ok("routed insert", simple("INSERT INTO mcp SELECT i, 'x'||i FROM generate_series(1,150) i"))
        expect_ok("parent select", simple("SELECT count(*) FROM mcp"))
        expect_ok("create list", simple("CREATE TABLE mcl (a INT, b TEXT) PARTITION BY LIST (b)"))
        expect_ok("list partition", simple("CREATE TABLE mcl1 PARTITION OF mcl FOR VALUES IN ('a','b')"))
        expect_ok("list insert", simple("INSERT INTO mcl VALUES (1,'a'),(2,'b')"))
        expect_ok("create hash", simple("CREATE TABLE mch (a INT) PARTITION BY HASH (a)"))
        expect_ok("hash p0", simple("CREATE TABLE mch0 PARTITION OF mch FOR VALUES WITH (MODULUS 2, REMAINDER 0)"))
        expect_ok("hash p1", simple("CREATE TABLE mch1 PARTITION OF mch FOR VALUES WITH (MODULUS 2, REMAINDER 1)"))
        expect_ok("hash insert", simple("INSERT INTO mch SELECT generate_series(1,50)"))

        # ---- C: trigger during routing ----
        expect_ok("trig func", simple("""CREATE FUNCTION mctrigf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN new.b := new.b || '!'; RETURN new; END; $$"""))
        expect_ok("trig create", simple("CREATE TRIGGER mctrig BEFORE INSERT ON mcp1 FOR EACH ROW EXECUTE FUNCTION mctrigf()"))
        expect_ok("trig insert", simple("INSERT INTO mcp VALUES (50, 'trig')"))
        expect_ok("trig verify", simple("SELECT b FROM mcp1 WHERE a = 50"))

        # ---- D: error paths ----
        expect_err("overlap", simple("CREATE TABLE mcover PARTITION OF mcp FOR VALUES FROM (50) TO (150)"), "23514")
        expect_err("no partition", simple("INSERT INTO mcp VALUES (999, 'x')"), "23514")
        expect_err("leaf violation", simple("INSERT INTO mcp1 VALUES (500, 'x')"), "23514")

        # ---- E: relkind ----
        expect_ok("relkind p", simple("SELECT relkind FROM pg_class WHERE relname='mcp'"))
        expect_ok("relkind r", simple("SELECT relkind FROM pg_class WHERE relname='mcp1'"))

        # ---- F: checkpoint + clean shutdown ----
        expect_ok("checkpoint", simple("CHECKPOINT"))
        s.close()
        proc.terminate()
        try:
            rc = proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill(); rc = proc.wait()
        print(f"phase1: nfail={nfail} rc={rc}")
        if rc == 99:
            print("VALGRIND MEMCHECK ERRORS (see vg.log)")
            sys.exit(1)

        # ---- phase 2: restart on same datadir (WAL replay) ----
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
        expect_ok("replay count", simple("SELECT count(*) FROM mcp"))
        expect_ok("replay relkind", simple("SELECT relkind FROM pg_class WHERE relname='mcp'"))
        expect_ok("post-restart insert", simple("INSERT INTO mcp VALUES (10, 'after')"))
        s.close()
        proc.terminate()
        try:
            rc = proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill(); rc = proc.wait()
        print(f"phase2: nfail={nfail} rc={rc}")
        sys.exit(1 if (nfail or rc == 99) else 0)
    finally:
        try:
            proc.kill()
        except Exception:
            pass

if __name__ == "__main__":
    main()
