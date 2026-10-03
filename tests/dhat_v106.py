#!/usr/bin/env python3
"""dhat_v106.py — DHAT heap profile for the v1.06 PGLZ paths.

Inserts and reads wide compressible values (pglz_compress allocations:
histogram i16 table, output Vec growth) plus boundary inputs and
updates. Inspect the log for allocation growth vs the v105 baseline.
"""
import os, socket, struct, subprocess, sys, tempfile, time, random

PORT = 5602
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat106_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "dhat.log")
    proc = subprocess.Popen(
        [VG, "--tool=dhat", f"--log-file={log}",
         f"--dhat-out-file={os.path.join(data_dir, 'dhat.out')}",
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
            s.sendall(b"Q" + struct.pack("!i", 5 + len(sql)) + sql.encode() + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z": break

        random.seed(106)
        words = ["alpha", "beta", "gamma", "delta", "epsilon"] * 40
        def doc(n):
            return " ".join(random.choice(words) for _ in range(n // 6))[:n]

        q("CREATE TABLE dh106(a int, b text)")
        for i in range(40):
            q(f"INSERT INTO dh106 VALUES ({i}, '{doc(8000)}')")
        for i in range(40):
            q(f"SELECT b FROM dh106 WHERE a = {i}")
        for i in range(10):
            q(f"UPDATE dh106 SET b = '{'q' * 300}' WHERE a = {i}")
        q("SELECT count(*) FROM dh106")
        s.close()
        print(f"dhat v106 workload done; log in {data_dir}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()

if __name__ == "__main__":
    main()
