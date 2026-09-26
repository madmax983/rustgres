#!/usr/bin/env python3
"""memcheck_v103.py — valgrind memcheck for the v1.03 DECLARE/:=/FOR/RETURN NEXT/SETOF/EXPLAIN ANALYZE paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; CREATE FUNCTION bodies with
           DECLARE/:= (scalar), FOR over SELECT with RETURN NEXT
           (SETOF), FOR over EXPLAIN (ANALYZE) (the subselect.sql
           REAL-FAIL shape), top-level EXPLAIN (ANALYZE), validation
           errors, CHECKPOINT.
  phase 2: restart on the same datadir under valgrind (WAL replay
           rebuilds the parsed plpgsql bodies with DECLARE/FOR via
           rebuild_function_bodies); calls again.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5599

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc103_",
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

        # ---- A: scalar DECLARE/:= ----
        expect_ok("create add", simple("""CREATE FUNCTION mc_add(a int, b int) RETURNS int AS $$
declare s int;
begin
  s := a + b;
  return s;
end;
$$ LANGUAGE plpgsql"""))
        expect_ok("call add", simple("SELECT mc_add(3, 4)"))
        expect_ok("create dbl", simple("""CREATE FUNCTION mc_dbl(x int) RETURNS int AS $$
declare y int;
begin
  y := x * 2;
  return y + x;
end;
$$ LANGUAGE plpgsql"""))
        expect_ok("call dbl", simple("SELECT mc_dbl(5)"))

        # ---- B: SETOF FOR/RETURN NEXT ----
        expect_ok("create gen", simple("""CREATE FUNCTION mc_gen(n int) RETURNS SETOF int AS $$
declare i int;
begin
  for i in select * from generate_series(1, n) loop
    return next i * 10;
  end loop;
end;
$$ LANGUAGE plpgsql"""))
        expect_ok("call gen", simple("SELECT * FROM mc_gen(5)"))

        # ---- C: FOR over EXPLAIN (ANALYZE) — subselect.sql REAL-FAIL ----
        expect_ok("create t", simple("CREATE TABLE mc_sq (pk int primary key, c1 int, c2 int)"))
        expect_ok("insert t", simple("INSERT INTO mc_sq VALUES (1,1,1),(2,2,2),(3,3,3),(4,4,4),(5,1,1),(6,2,2),(7,3,3),(8,4,4)"))
        expect_ok("create expl", simple("""CREATE FUNCTION mc_expl() RETURNS SETOF text LANGUAGE plpgsql AS $$
declare ln text;
begin
    for ln in
        explain (analyze, summary off, timing off, costs off, buffers off)
        select * from (select pk,c2 from mc_sq order by c1,pk) as x limit 3
    loop
        return next ln;
    end loop;
end;
$$"""))
        expect_ok("call expl", simple("SELECT * FROM mc_expl()"))
        expect_ok("explain analyze", simple("EXPLAIN (ANALYZE) SELECT * FROM mc_sq"))
        expect_ok("explain plain", simple("EXPLAIN SELECT * FROM mc_sq"))

        # ---- D: validation errors (must not crash) ----
        expect_err("return next scalar", simple("CREATE FUNCTION mc_rn() RETURNS int AS $$ begin return next 1; end; $$ LANGUAGE plpgsql"), "42601")
        expect_err("return in setof", simple("CREATE FUNCTION mc_r() RETURNS SETOF int AS $$ begin return 1; end; $$ LANGUAGE plpgsql"), "42601")
        expect_err("undeclared", simple("CREATE FUNCTION mc_u() RETURNS int AS $$ declare x int; begin y := 1; return x; end; $$ LANGUAGE plpgsql"), "42601")

        expect_ok("checkpoint", simple("CHECKPOINT"))
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    # Phase 2: restart under valgrind
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
            got_err = None
            while True:
                t, p = msg2()
                if t == b"E":
                    got_err = "E"
                elif t == b"Z":
                    break
            return got_err
        if simple2("SELECT mc_add(10, 20)"):
            nfail += 1; print("phase2: mc_add failed")
        if simple2("SELECT * FROM mc_gen(3)"):
            nfail += 1; print("phase2: mc_gen failed")
        if simple2("SELECT * FROM mc_expl()"):
            nfail += 1; print("phase2: mc_expl failed")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    for log in (log1, log2):
        txt = open(log).read()
        errs = txt.count("ERROR SUMMARY")
        print(f"{os.path.basename(log)}: {'FAIL' if nfail else 'OK'} sql_errors={nfail}")
        # Count valgrind errors
        import re
        m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
        if m:
            print(f"  valgrind errors: {m.group(1)}")
            if m.group(1) != "0":
                sys.exit(1)
    sys.exit(1 if nfail else 0)

if __name__ == "__main__":
    main()
