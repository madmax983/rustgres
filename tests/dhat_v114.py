#!/usr/bin/env python3
"""dhat_v114.py — DHAT heap profile for the v1.14 FOR UPDATE OF paths."""
import os, socket, struct, subprocess, sys, tempfile, time, re

PORT = 5610
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat114_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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

        q("CREATE TABLE dhat114t1 (a int, b int);")
        q("CREATE TABLE dhat114t2 (a int, c int);")
        q("INSERT INTO dhat114t1 SELECT g, g*10 FROM generate_series(1, 100) g;")
        q("INSERT INTO dhat114t2 SELECT g, g*100 FROM generate_series(1, 100) g;")
        for _ in range(20):
            q("SELECT dhat114t1.a FROM dhat114t1, dhat114t2 WHERE dhat114t1.a = dhat114t2.a FOR UPDATE OF dhat114t1;")
            q("SELECT x.a FROM dhat114t1 AS x FOR UPDATE OF x;")
            q("SELECT dhat114t1.a FROM dhat114t1, dhat114t2 FOR UPDATE OF dhat114t1, dhat114t2;")
            q("SELECT a FROM dhat114t1 FOR UPDATE;")
        q("DROP TABLE dhat114t1;")
        q("DROP TABLE dhat114t2;")
        s.close()
        proc.terminate(); proc.wait(timeout=120)
        with open(log) as f:
            txt = f.read()
        m = re.search(r"Total:\s+([\d,]+) bytes", txt)
        p = re.search(r"At t-end:\s+([\d,]+) bytes", txt)
        pk = re.search(r"At t-gmax:\s+([\d,]+) bytes", txt)
        print(f"dhat_v114: total={m.group(1) if m else '?'} "
              f"t-end={p.group(1) if p else '?'} "
              f"peak={pk.group(1) if pk else '?'}")
    finally:
        try: proc.kill()
        except Exception: pass


if __name__ == "__main__":
    main()
