#!/usr/bin/env python3
"""dhat_v105.py — DHAT heap profile for the v1.05 TOAST UPDATE lifecycle
paths (eager cleanup, value-id reuse, rollback prune, upsert, partition
move, lazy relid link). Inspect the log for leak growth vs v104 baseline."""
import os, socket, struct, subprocess, sys, tempfile, time, random

PORT = 5601
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat105_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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

        random.seed(105)
        big = "".join(random.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(4000))
        big2 = "".join(random.choice("ZYXWVUTSRQPONMLKJIHGFEDCBA") for _ in range(4000))
        q("CREATE TABLE dh105(a int, b text);")
        q(f"INSERT INTO dh105 VALUES (1, '{big}');")
        for i in range(10):
            # reuse path: unchanged toasted column keeps value id
            q(f"UPDATE dh105 SET b = '{big}' WHERE a = 1;")
            # changed path: eager chunk delete + new chunks
            q(f"UPDATE dh105 SET b = '{big2}' WHERE a = 1;")
            # rollback path: chunks + metadata restored/pruned
            q(f"BEGIN; UPDATE dh105 SET b = '{big}' WHERE a = 1; ROLLBACK;")
            # upsert path
            q("CREATE TEMP TABLE dhc(a int PRIMARY KEY, b text);")
            q(f"INSERT INTO dhc VALUES (1, '{big}');")
            q(f"INSERT INTO dhc VALUES (1, '{big2}') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b;")
            q("DROP TABLE dhc;")
        # partition-moving UPDATE chunk cleanup
        q("CREATE TABLE dhp(a int, b text) PARTITION BY LIST (a);")
        q("CREATE TABLE dhp1 PARTITION OF dhp FOR VALUES IN (1);")
        q("CREATE TABLE dhp2 PARTITION OF dhp FOR VALUES IN (2);")
        q(f"INSERT INTO dhp VALUES (1, '{big}');")
        q("UPDATE dhp SET a = 2 WHERE a = 1;")
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
    import re
    m = re.search(r"Total:\s+([\d,]+) bytes allocated", txt)
    if m:
        print(f"  total allocated: {m.group(1)}")
    print("dhat_v105: done (inspect log for leak growth vs v104 baseline)")

if __name__ == "__main__":
    main()
