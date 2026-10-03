#!/usr/bin/env python3
"""cg_v106.py — callgrind workload for the v1.06 PGLZ byte-compat paths.

Exercises the hot path: INSERT/SELECT of wide compressible values
(pglz_compress on multi-KB inputs + framed decompress on read), the
tag-boundary inputs (273-byte max matches), and UPDATE of toasted
values.
"""
import os, socket, struct, subprocess, sys, tempfile, time, random

PORT = 5588
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgcg106_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "cg.log")
    proc = subprocess.Popen(
        [VG, "--tool=callgrind", "--callgrind-out-file=" + os.path.join(data_dir, "cg.out"),
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
            print("server did not start under callgrind"); proc.kill(); sys.exit(2)
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
        words = ["lorem", "ipsum", "dolor", "sit", "amet"] * 40
        def doc(n):
            return " ".join(random.choice(words) for _ in range(n // 6))[:n]

        q("CREATE TABLE cg106(a int, b text)")
        # hot loop: wide compressible inserts + reads (pglz_compress hot path)
        for i in range(60):
            v = doc(6000)
            q(f"INSERT INTO cg106 VALUES ({i}, '{v}')")
        for i in range(60):
            q(f"SELECT b FROM cg106 WHERE a = {i}")
        # tag-boundary inputs: long runs (max 273-byte matches)
        for i in range(20):
            q(f"INSERT INTO cg106 VALUES ({100+i}, '{'z' * 5000}')")
        for i in range(20):
            q(f"SELECT b FROM cg106 WHERE a = {100+i}")
        # updates of toasted values
        for i in range(20):
            q(f"UPDATE cg106 SET b = '{doc(6000)}' WHERE a = {i}")
        q("SELECT count(*) FROM cg106")
        s.close()
        print(f"callgrind v106 workload done; out in {data_dir}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()

if __name__ == "__main__":
    main()
