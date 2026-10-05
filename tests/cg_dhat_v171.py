#!/usr/bin/env python3
"""cg_dhat_v171.py — v1.71: callgrind + dhat on EC join-filter workload."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5572
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
TOOL = sys.argv[1] if len(sys.argv) > 1 else "callgrind"

STMTS = [
    "create table sj (a int, b int)",
    "insert into sj select g, g % 7 from generate_series(1, 500) g",
    "create table j1 (id1 int, id2 int, primary key(id1,id2))",
    "create table j2 (id1 int, id2 int, primary key(id1,id2))",
    "insert into j1 select g, g % 11 from generate_series(1, 200) g",
    "insert into j2 select g, g % 13 from generate_series(1, 200) g",
    "explain (costs off) select * from sj t1, sj t2 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a",
    "explain (costs off) select * from sj t1, sj t2 where t1.a = t1.b and t1.b = t2.b and t2.b = t2.a",
    "explain (verbose, costs off) select * from j1 inner join j2 on j1.id1 = j2.id1 and j1.id2 = j2.id2",
    "explain (verbose, costs off) select * from j1 inner join j2 on j1.id1 = j2.id1 and j1.id2 = j2.id2",
    "create unique index j1_id2_idx on j1(id2) where id2 > 0",
    "explain (verbose, costs off) select * from j1 inner join j2 on j1.id2 = j2.id2",
    "explain (verbose, costs off) select * from j1 inner join j2 on j1.id2 = j2.id2",
]

def main():
    data_dir = tempfile.mkdtemp(prefix="rgcg171_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    out = os.path.join(data_dir, f"{TOOL}.out")
    args = [VG, f"--tool={TOOL}", f"--log-file={out}"]
    if TOOL == "callgrind":
        args.append("--callgrind-out-file=" + os.path.join(data_dir, "callgrind.out"))
    proc = subprocess.Popen(
        args + [BIN, "--data-dir", data_dir, "--port", str(PORT)],
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
            print("server did not start"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
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
        try: proc.wait(timeout=120)
        except: proc.kill()
    print(f"{TOOL} done: {data_dir}")
    print(open(out).read()[-2000:])

if __name__ == "__main__":
    main()
