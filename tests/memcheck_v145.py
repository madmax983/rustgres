#!/usr/bin/env python3
"""memcheck_v145.py — v1.45: run EXPLAIN option parsing and useless-LEFT-JOIN
removal paths under valgrind memcheck.

Exercises: EXPLAIN (option, ...) parsing/validation (all boolean forms,
SERIALIZE/FORMAT values, cross-option 22023s, 0A000 for non-text FORMAT),
and the plan_select join-removal rewrite (simple removable case, chained
kept case, subquery case). Uses the DEBUG binary.
"""
import os, socket, struct, subprocess, sys, tempfile, time
from collections import Counter

PORT = 5549
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "create table mca(id int primary key, b_id int)",
    "create table mcb(id int primary key, c_id int)",
    "insert into mca select g, g % 10 from generate_series(1, 200) g",
    "insert into mcb select g, g % 5 from generate_series(1, 50) g",
    # v1.45 option parsing paths
    "explain (costs off) select a.* from mca a left join mcb b on a.b_id = b.id",
    "explain (verbose, costs off) select 1",
    "explain (analyze, costs off) select * from mca",
    "explain (buffers, wal, timing, io, analyze) select * from mca",
    "explain (serialize binary, analyze) select * from mca",
    "explain (generic_plan) select * from mca",
    # v1.45 join-removal paths (removable + kept)
    "explain (costs off) select a.* from mca a left join mcb b on a.b_id = b.id where a.b_id > 1",
    "explain (costs off) select * from mca a left join mcb b on a.b_id = b.id",
    "explain (costs off) select a.* from mca a left join mcb b on a.b_id = b.c_id",
    # validation error paths (must not leak)
    "explain (wal) select 1",
    "explain (serialize text) select 1",
    "explain (format bogus) select 1",
    "explain (format) select 1",
    "explain (generic_plan, analyze) select 1",
]

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc145_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
        s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
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
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
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

        errs = Counter()
        for q in STMTS:
            err = simple(q)
            if err:
                errs[err] += 1
        print(f"SQL error counts: {dict(errs)}")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except:
            proc.kill()
    logtxt = open(log).read()
    print("=== valgrind summary ===")
    for line in logtxt.split("\n"):
        if "definitely lost" in line or "possibly lost" in line or "ERROR SUMMARY" in line:
            print(line.strip())
    ok = ("definitely lost: 0 bytes in 0 blocks" in logtxt
          and "ERROR SUMMARY: 0 errors" in logtxt)
    print("memcheck_v145:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
