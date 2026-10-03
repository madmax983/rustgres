#!/usr/bin/env python3
"""dhat_v108.py — DHAT heap profile for the v1.08 EXPLAIN PG-text paths
(pg plan building, expression deparse strings, SIMILAR TO regex
construction, index-cond text assembly)."""
import os, socket, struct, subprocess, sys, tempfile, time, re

PORT = 5601
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat108_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
            print("server did not start under dhat"); proc.kill(); sys.exit(2)
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
        def q(sql):
            s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1) + sql.encode() + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z": break

        q("CREATE TABLE dh108(a int, b text);")
        q("CREATE INDEX dh108ai ON dh108(a);")
        q("INSERT INTO dh108 SELECT g, 'v'||g FROM generate_series(1,200) g;")
        for i in range(20):
            # PG-text plan building + deparse strings
            q(f"EXPLAIN (COSTS OFF) SELECT * FROM dh108 WHERE a = {i} AND b = 'v{i}';")
            q("EXPLAIN (COSTS OFF) SELECT * FROM dh108 WHERE a > 10 AND a < 190;")
            # SIMILAR TO regex construction
            q("EXPLAIN (COSTS OFF) SELECT * FROM dh108 WHERE b SIMILAR TO 'v[0-9]+';")
            # join filter / where splitting allocation
            q("EXPLAIN (COSTS OFF) SELECT * FROM dh108 x, dh108 y WHERE x.a = y.a AND x.a > 3;")
            # sort keys + aggregates
            q("EXPLAIN (COSTS OFF) SELECT count(*) FROM dh108 GROUP BY b ORDER BY 1;")
            # error paths (parse-level allocation)
            q("EXPLAIN (FOOBAR) SELECT 1;")
            # legacy COSTS ON path
            q("EXPLAIN SELECT * FROM dh108 WHERE a = 1;")
        q("CHECKPOINT;")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()
    txt = open(log).read()
    print(f"dhat log: {log} ({len(txt)} bytes)")
    m = re.search(r"Total:\s+([\d,]+) bytes allocated", txt)
    if m:
        print(f"  total allocated: {m.group(1)}")
    m2 = re.search(r"At t-end:\s+([\d,]+) bytes", txt)
    if m2:
        print(f"  at t-end: {m2.group(1)}")
    print("dhat_v108: done (inspect log for leak growth vs v107 baseline)")

if __name__ == "__main__":
    main()
