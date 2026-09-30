#!/usr/bin/env python3
"""valgrind_v127.py — valgrind for the v1.27 ProjectSet paths.

Exercises under valgrind:
  S1: parenthesized set operations in FROM / LATERAL
      (PG19 select_with_parens).
  S2: PG19 ProjectSet-over-Agg SRF fan-out — top-level and nested SRFs
      in grouped target lists, multi-SRF NULL padding, empty-SRF row
      drop, SRF-as-GROUP-BY-key, SRF args with aggregates, SRF-under-Agg
      42883, ORDER BY over fanned rows, and the row-wise-IN statement-A
      shape.

Usage: python3 tests/valgrind_v127.py [--tool memcheck|callgrind|dhat]
Default: memcheck (two phases; --error-exitcode=99, memory errors only).
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5612

WORKLOAD = [
    # S1: parenthesized setops in FROM
    ("q", "select * from (select null::int as c0 from ((select 1) union all (select 2))) t1 "
          "cross join (select null::int as c1 from ((select 1) union all (select 2))) t2", None),
    ("q", "select q1.v, q2.v from (select 1 as v) as q1 cross join lateral "
          "((select * from ((select 4 as v) union all (select 5 as v)) as q3) "
          "union all (select q1.v)) as q2 order by 1, 2", None),
    ("q", "select * from (select 1 union all select 2) t order by 1", None),
    ("q", "select * from ((select 1 as x)) ss order by 1", None),
    # S2: SRF fan-out over grouped rows
    ("q", "create table vg127_i4(f1 int)", None),
    ("q", "delete from vg127_i4", None),
    ("q", "insert into vg127_i4 values (0),(123456),(-123456),(2147483647),(-2147483647)", None),
    ("q", "select * from vg127_i4 o where (f1, f1) in "
          "(select f1, generate_series(1,50) / 10 g from vg127_i4 i group by f1)", None),
    ("q", "select x, generate_series(1,3) from (values (1),(2)) t(x) group by x order by 1, 2", None),
    ("q", "select x, generate_series(1,6)/2 g from (values (1)) t(x) group by x order by 1, 2", None),
    ("q", "select x, case when x > 1 then generate_series(1,2) else -1 end g "
          "from (values (1),(2)) t(x) group by x order by 1, 2", None),
    ("q", "select generate_series(1,2), generate_series(10,12) from (values (1)) t(x) group by x", None),
    ("q", "select generate_series(2,1) from (values (1)) t(x) group by x", None),
    ("q", "select generate_series(1,3) g from (values (1)) t(x) group by generate_series(1,3) order by 1", None),
    ("q", "select x, generate_series(1, max(x)) from (values (1),(2)) t(x) group by x order by 1, 2", None),
    ("q", "select x, generate_series(1,3) g from (values (1),(2)) t(x) group by x "
          "order by x, generate_series(1,3)", None),
    ("q", "select sum(generate_series(1,3)) from (values (1)) t(x) group by x", "42883"),
    ("q", "select x, generate_series(1,2) g, "
          "(select string_agg(y::text, ',') from (select generate_series(1,2) y) s) m "
          "from (values (1)) t(x) group by x order by 1, 2", None),
]


def run_workload(simple, expect_ok, expect_err, phase=1):
    for name, sql, want in WORKLOAD:
        # Phase 2 replays on the same datadir: the table already exists
        # from phase 1; just clear it.
        if phase == 2 and sql.startswith("create table"):
            continue
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    tool = "memcheck"
    if "--tool" in sys.argv:
        tool = sys.argv[sys.argv.index("--tool") + 1]
    data_dir = tempfile.mkdtemp(prefix="rgvg127_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))

    def start_server(log):
        args = [VG, f"--tool={tool}"]
        if tool == "memcheck":
            args += ["--error-exitcode=99", "--errors-for-leak-kinds=none"]
        args += [f"--log-file={log}", BIN, "--data-dir", data_dir, "--port", str(PORT)]
        return subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def wait_ready(proc):
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                return True
            except OSError:
                time.sleep(0.5)
        return False

    import re as _re

    def simple(sql):
        s = socket.create_connection(("127.0.0.1", PORT), timeout=300)
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

    nfail = [0]

    def expect_ok(name, codes, sql):
        if codes:
            print(f"UNEXPECTED-ERR {name}: {codes} :: {sql[:80]}")
            nfail[0] += 1

    def expect_err(name, codes, want, sql):
        if codes != [want]:
            print(f"WRONG-ERR {name}: got {codes} want [{want}] :: {sql[:80]}")
            nfail[0] += 1

    log1 = os.path.join(data_dir, "vg.log")
    proc = start_server(log1)
    try:
        if not wait_ready(proc):
            print("server did not start under valgrind")
            proc.kill()
            sys.exit(2)
        run_workload(simple, expect_ok, expect_err, phase=1)
        print(f"phase 1: {nfail[0]} workload failures")
        proc.terminate()
        proc.wait(timeout=180)
    finally:
        if proc.poll() is None:
            proc.kill()

    # phase 2: WAL replay on the same datadir (memcheck only)
    if tool == "memcheck":
        log2 = os.path.join(data_dir, "vg2.log")
        proc2 = start_server(log2)
        try:
            if not wait_ready(proc2):
                print("phase 2: server did not start under valgrind")
                proc2.kill()
                sys.exit(2)
            run_workload(simple, expect_ok, expect_err, phase=2)
            print(f"phase 2: {nfail[0]} workload failures")
            proc2.terminate()
            proc2.wait(timeout=180)
        finally:
            if proc2.poll() is None:
                proc2.kill()

    # report valgrind errors
    for lf in (log1, os.path.join(data_dir, "vg2.log") if tool == "memcheck" else None):
        if lf and os.path.exists(lf):
            with open(lf, errors="replace") as f:
                content = f.read()
            n_err = content.count("ERROR SUMMARY:")
            print(f"{os.path.basename(lf)}: {n_err} ERROR SUMMARY sections")
            if "ERROR SUMMARY: 0 errors" not in content:
                for line in content.splitlines():
                    if "ERROR SUMMARY:" in line or "definitely lost" in line:
                        print("   ", line.strip())
    sys.exit(1 if nfail[0] else 0)


if __name__ == "__main__":
    main()
