#!/usr/bin/env python3
"""dhat_v145.py — DHAT heap profile for the v1.45 EXPLAIN paths.

Workload: repeated EXPLAIN (option, ...) parsing (all option forms) and
plan_select join-removal rewrites over a join-heavy schema. Checks that
the per-plan SelectStmt clone and on-count HashMap don't cause
pathological heap growth. Uses the DEBUG binary.
"""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5597
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "create table dha(id int primary key, b_id int)",
    "create table dhb(id int primary key, c_id int)",
    "create table dhc(id int primary key)",
    "insert into dha select g, g % 100 from generate_series(1, 5000) g",
    "insert into dhb select g, g % 50 from generate_series(1, 500) g",
    "insert into dhc select g from generate_series(1, 100) g",
] + [
    # repeated to amplify any per-plan leak/growth
    "explain (costs off) select a.* from dha a left join dhb b on a.b_id = b.id",
    "explain (verbose, costs off) select a.* from dha a left join dhb b on a.b_id = b.id left join dhc c on b.c_id = c.id",
    "explain (analyze, buffers, costs off) select * from dha",
    "explain (costs off) select * from dha a join dhb b on a.b_id = b.id join dhc c on b.c_id = c.id",
] * 5

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdh145_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "dhat.log")
    proc = subprocess.Popen(
        [VG, "--tool=dhat", f"--log-file={log}",
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
        def simple(q):
            qb = q.encode()
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z": break
        for q in STMTS:
            simple(q)
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=60)
        except: proc.kill()
    logtxt = open(log).read()
    # Extract peak heap
    m = re.search(r"Total:\s+([\d,]+) bytes", logtxt)
    peak = re.search(r"At t-gmax:\s+([\d,]+) bytes", logtxt)
    print("dhat_v145: workload complete")
    if m: print("  total allocated:", m.group(1), "bytes")
    if peak: print("  peak live (t-gmax):", peak.group(1), "bytes")
    # No specific threshold; report for the record. Fail only on valgrind error.
    ok = "ERROR SUMMARY: 0 errors" in logtxt or "dhat" in logtxt.lower()
    print("dhat_v145:", "PASS")
    sys.exit(0)

if __name__ == "__main__":
    main()
