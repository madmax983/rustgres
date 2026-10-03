#!/usr/bin/env python3
"""memcheck_v102.py — valgrind memcheck for the v1.02 RAISE NOTICE /
RAISE EXCEPTION paths (bounded plpgsql).

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; CREATE FUNCTION bodies with
           RAISE NOTICE/RAISE EXCEPTION (format args, named-arg
           rewrite, handlers, unsupported levels), calls that emit
           notices, trapped and untrapped exceptions, CHECKPOINT.
  phase 2: restart on the same datadir under valgrind (WAL replay
           rebuilds the parsed plpgsql bodies with RAISE args via
           rebuild_function_bodies); calls again; notices must flow.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5598

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc102_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log1 = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log1}",
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

        # ---- A: functions with RAISE (notice + exception + formats) ----
        expect_ok("create table", simple("CREATE TABLE mct2 (x int, y int)"))
        expect_ok("create tattle", simple("""CREATE FUNCTION mc_tattle(x int, y int) RETURNS bool AS $$
BEGIN
  RAISE NOTICE 'x = %, y = %', x, y;
  RETURN x > y;
END$$ LANGUAGE plpgsql VOLATILE"""))
        expect_ok("create multi", simple("""CREATE FUNCTION mc_multi(a int, b text) RETURNS int AS $$
BEGIN
  RAISE NOTICE '100%% of %', a;
  RAISE NOTICE 'pair: %, %', a, b;
  RAISE NOTICE 'null: %', NULL;
  RETURN a;
END$$ LANGUAGE plpgsql VOLATILE"""))
        expect_ok("create boom", simple("""CREATE FUNCTION mc_boom() RETURNS int AS $$
BEGIN
  RAISE EXCEPTION 'kaput %', 42;
  RETURN 1;
END$$ LANGUAGE plpgsql VOLATILE"""))
        expect_ok("create trap", simple("""CREATE FUNCTION mc_trap() RETURNS int AS $$
BEGIN
  RAISE NOTICE 'before';
  RAISE EXCEPTION 'nope';
  RETURN 1;
EXCEPTION WHEN raise_exception THEN
  RAISE NOTICE 'caught';
  RETURN -1;
END$$ LANGUAGE plpgsql VOLATILE"""))

        # ---- B: notice-emitting calls (the notice sink path) ----
        for i in range(25):
            expect_ok("tattle call", simple(f"SELECT mc_tattle({i}, {i % 7})"))
            expect_ok("multi call", simple(f"SELECT mc_multi({i}, 's{i}')"))
        expect_ok("insert tattle", simple("INSERT INTO mct2 VALUES (mc_tattle(1, 2)::int, 3)"))
        expect_ok("trap call", simple("SELECT mc_trap()"))

        # ---- C: exception paths (P0001 propagation, trapped and not) ----
        expect_err("untrapped", simple("SELECT mc_boom()"), "P0001")

        # ---- D: honest error paths ----
        expect_err("0A000 level", simple(
            "CREATE FUNCTION mc_b1() RETURNS int AS 'BEGIN RAISE LOG ''x''; RETURN 1; END' LANGUAGE plpgsql"), "0A000")
        expect_ok("empty format is legal", simple(
            "CREATE FUNCTION mc_b2() RETURNS int AS 'BEGIN RAISE NOTICE ''''; RETURN 1; END' LANGUAGE plpgsql"))
        expect_ok("empty format call", simple("SELECT mc_b2()"))

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
        expect_ok("replay tattle", simple("SELECT mc_tattle(9, 8)"))
        expect_ok("replay multi", simple("SELECT mc_multi(1, 'x')"))
        expect_ok("replay trap", simple("SELECT mc_trap()"))
        expect_err("replay boom", simple("SELECT mc_boom()"), "P0001")
        expect_ok("replay rows", simple("SELECT count(*) FROM mct2"))
        expect_ok("post-restart insert", simple("INSERT INTO mct2 VALUES (1, mc_tattle(3, 4)::int)"))
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
