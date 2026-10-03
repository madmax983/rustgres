#!/usr/bin/env python3
"""dhat_v110.py — DHAT heap profile for the v1.10 LATERAL paths
(per-left-row subquery evaluation scopes, static schema vectors,
lateral VALUES row expansion)."""
import os, socket, struct, subprocess, sys, tempfile, time, re

PORT = 5604
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat110_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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

        q("CREATE TABLE dh110t (a int, b int);")
        q("INSERT INTO dh110t SELECT g, g * 10 FROM generate_series(1, 50) g;")
        for i in range(20):
            # comma LATERAL (SELECT ...) correlated (per-row scopes)
            q("SELECT t.a, s.y FROM dh110t t, LATERAL (SELECT t.a + t.b AS y) AS s;")
            # LATERAL (VALUES ...) row expansion
            q("SELECT t.a, v.x FROM dh110t t, LATERAL (VALUES (t.a * 10), (t.b)) AS v(x);")
            # LEFT JOIN LATERAL null-extension
            q("SELECT t.a, s.y FROM dh110t t LEFT JOIN LATERAL (SELECT t.b AS y WHERE t.b > 10000) AS s ON true;")
            # nested LATERAL
            q("SELECT t.a, s1.x, s2.y FROM dh110t t, LATERAL (SELECT t.a + 1 AS x) AS s1, LATERAL (SELECT s1.x + 1 AS y) AS s2 WHERE t.a < 5;")
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

if __name__ == "__main__":
    main()
