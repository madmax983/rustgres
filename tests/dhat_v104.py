#!/usr/bin/env python3
"""dhat_v104.py — DHAT heap profile for the v1.04 multi-statement
simple-Query implicit-transaction paths (implicit Txn allocation,
per-statement result framing, warning queue, whole-Q rollback)."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5600
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat104_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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

        q("CREATE TABLE dh104(a int);")
        q("INSERT INTO dh104 SELECT g FROM generate_series(1,100) g;")
        for i in range(20):
            # repeated result sets + implicit commit
            q(f"SELECT {i}; SELECT * FROM dh104 WHERE a = {i % 100 + 1};")
            # mid-Q error: whole-Q rollback path
            q("INSERT INTO dh104 VALUES (9999); SELECT 1/0;")
            # COMMIT-in-block warning + fresh block
            q("SELECT 1; COMMIT; SELECT 2;")
            # BEGIN conversion path
            q("BEGIN; INSERT INTO dh104 VALUES (7777); ROLLBACK;")
        q("CHECKPOINT;")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()
    txt = open(log).read()
    print(f"dhat log: {log} ({len(txt)} bytes)")
    # Look for obvious leak growth: total allocated vs freed
    import re
    m = re.search(r"Total:\s+([\d,]+) bytes allocated", txt)
    if m:
        print(f"  total allocated: {m.group(1)}")
    print("dhat_v104: done (inspect log for leak growth vs v103 baseline)")

if __name__ == "__main__":
    main()
