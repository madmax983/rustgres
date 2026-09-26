#!/usr/bin/env python3
"""memcheck_v101.py — run the v1.01 plpgsql EXCEPTION paths under valgrind memcheck.

Exercises:
  A. CREATE FUNCTION with statement sequences + EXCEPTION/WHEN blocks.
  B. Trapped errors (division_by_zero -> handler), untrapped propagation.
  C. Category (22xxx) / OTHERS / SQLSTATE-literal handlers, first-match.
  D. Error paths: unsupported statements (0A000), unknown condition
     (42704), malformed RETURN (42601).
  E. Restart: checkpoint, shutdown, restart, verify (WAL replay rebuilds
     the parsed plpgsql body via rebuild_function_bodies).

Two-phase: phase 1 runs the workload, checkpoints, and shuts down cleanly;
phase 2 restarts the server on the same datadir (WAL replay) and verifies.
Valgrind runs with --errors-for-leak-kinds=none (no leak checking, only
memory errors); exit code 99 on any memcheck error.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5550
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc101_")
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

        # ---- A: create table + exception-capable functions ----
        expect_ok("create table", simple("CREATE TABLE mct (c float8)"))
        expect_ok("create inverse", simple("""CREATE FUNCTION mc_inv(int) RETURNS float8 AS $$
BEGIN
  ANALYZE mct;
  RETURN 1::float8/$1;
EXCEPTION
  WHEN division_by_zero THEN RETURN 0;
END$$ LANGUAGE plpgsql VOLATILE"""))
        expect_ok("create cat", simple(
            "CREATE FUNCTION mc_cat() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN data_exception THEN RETURN 7; END' LANGUAGE plpgsql"))
        expect_ok("create others", simple(
            "CREATE FUNCTION mc_oth() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN OTHERS THEN RETURN 9; END' LANGUAGE plpgsql"))
        expect_ok("create named", simple(
            "CREATE FUNCTION mc_named(x int) RETURNS int AS 'BEGIN RETURN x + 1; EXCEPTION WHEN OTHERS THEN RETURN 0; END' LANGUAGE plpgsql"))

        # ---- B: trapped + untrapped calls ----
        for i in range(30):
            expect_ok("trapped call", simple(f"SELECT mc_inv(0)"))
            expect_ok("plain call", simple(f"SELECT mc_inv({i + 1})"))
        expect_ok("insert trapped", simple("INSERT INTO mct VALUES (mc_inv(0))"))
        expect_ok("cat call", simple("SELECT mc_cat()"))
        expect_ok("others call", simple("SELECT mc_oth()"))
        expect_ok("named call", simple("SELECT mc_named(41)"))
        expect_err("untrapped div0", simple("SELECT mc_named(1/0)"), "22012")

        # ---- C: first-match + handler-error propagation ----
        expect_ok("create first", simple(
            "CREATE FUNCTION mc_first() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN division_by_zero THEN RETURN 1; WHEN OTHERS THEN RETURN 2; END' LANGUAGE plpgsql"))
        expect_ok("first call", simple("SELECT mc_first()"))
        expect_ok("create herr", simple(
            "CREATE FUNCTION mc_herr() RETURNS int AS 'BEGIN RETURN 1/0; EXCEPTION WHEN division_by_zero THEN RETURN 1/0; END' LANGUAGE plpgsql"))
        expect_err("handler err", simple("SELECT mc_herr()"), "22012")

        # ---- D: honest error paths ----
        expect_err("0A000 assign", simple(
            "CREATE FUNCTION mc_b1() RETURNS int AS 'BEGIN x := 1; RETURN 1; END' LANGUAGE plpgsql"), "0A000")
        expect_err("42704 cond", simple(
            "CREATE FUNCTION mc_b2() RETURNS int AS 'BEGIN RETURN 1; EXCEPTION WHEN nosuch THEN RETURN 0; END' LANGUAGE plpgsql"), "42704")
        expect_err("42601 ret", simple(
            "CREATE FUNCTION mc_b3() RETURNS int AS 'BEGIN RETURN; END' LANGUAGE plpgsql"), "42601")

        # ---- E: checkpoint + clean shutdown ----
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
        expect_ok("replay trapped", simple("SELECT mc_inv(0)"))
        expect_ok("replay named", simple("SELECT mc_named(41)"))
        expect_ok("replay rows", simple("SELECT count(*) FROM mct"))
        expect_ok("post-restart insert", simple("INSERT INTO mct VALUES (mc_inv(2))"))
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
