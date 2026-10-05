#!/usr/bin/env python3
"""memcheck_v171.py — v1.71: run EC-canonical join-filter paths under
valgrind memcheck.

Exercises: the PgEc union-find construction (ON + WHERE conjuncts),
pg_ec_member resolution (qualified/unqualified/ambiguous), the partial-index
orientation flip, and pg_ec_join_filter_text rendering — via EXPLAIN on
nested-loop joins. Uses the DEBUG binary.
"""
import os, socket, struct, subprocess, sys, tempfile, time
from collections import Counter

PORT = 5571
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "create table sj (a int, b int)",
    "insert into sj select g, g % 7 from generate_series(1, 200) g",
    "create table j1 (id1 int, id2 int, primary key(id1,id2))",
    "create table j2 (id1 int, id2 int, primary key(id1,id2))",
    "insert into j1 select g, g % 11 from generate_series(1, 100) g",
    "insert into j2 select g, g % 13 from generate_series(1, 100) g",
    # v1.71 EC paths: transitive WHERE EC (target 1)
    "explain (costs off) select * from sj t1, sj t2 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a",
    # 3-way transitive EC (target 2)
    "explain (costs off) select * from sj t1, sj t2, sj t3 where t1.a = t2.a and t2.a = t3.a and t1.b = t2.b and t2.b = t3.b",
    # ON-clause EC, no rewrite (case A must keep originals)
    "explain (verbose, costs off) select * from j1 inner join j2 on j1.id1 = j2.id1 and j1.id2 = j2.id2",
    # self-join guard (overlapping qualifiers -> no rewrite)
    "explain (costs off) select * from sj t1, sj t1 where t1.a = t1.b",
    # non-equi join filter (no EC union)
    "explain (costs off) select * from sj t1, sj t2 where t1.a > t2.a and t1.b = t2.b",
    # unqualified columns (member resolution via split)
    "explain (costs off) select * from sj t1, sj t2 where a = b",
    # partial index orientation flip (target 3)
    "create unique index j1_id2_idx on j1(id2) where id2 > 0",
    "explain (verbose, costs off) select * from j1 inner join j2 on j1.id2 = j2.id2",
    "drop index j1_id2_idx",
]

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc171_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--leak-check=full",
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
            proc.wait(timeout=120)
        except:
            proc.kill()
    logtxt = open(log).read()
    print("=== valgrind summary ===")
    for line in logtxt.split("\n"):
        if ("definitely lost" in line or "indirectly lost" in line
                or "possibly lost" in line or "ERROR SUMMARY" in line
                or "Invalid read" in line or "Invalid write" in line):
            print(line.strip())
    # v1.71 gate (standing standard): 0 definitely lost, 0 invalid
    # accesses. "Possibly lost" is the pre-existing storage-layer
    # interior-pointer class (see v1.36 notes), not a regression.
    ok = ("definitely lost: 0 bytes in 0 blocks" in logtxt
          and "Invalid read" not in logtxt
          and "Invalid write" not in logtxt)
    print("memcheck_v171:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
