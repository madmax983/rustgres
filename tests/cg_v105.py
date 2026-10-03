#!/usr/bin/env python3
"""cg_v105.py — callgrind workload for v1.05 TOAST UPDATE lifecycle paths.

Exercises the hot path: UPDATE with eager toast cleanup + value-id reuse
over a table with wide toasted columns, plus rollback and upsert.
"""
import os, socket, struct, subprocess, sys, tempfile, time, random

PORT = 5587
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgcg105_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
                if t == b"Z": return

        random.seed(105)
        big = "".join(random.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(3000))
        # bulk insert with toasted values
        vals = ",".join(f"({i},'{big[:100]}{i:06d}')" for i in range(1, 501))
        q("CREATE TABLE cg105(a int, b text);")
        q(f"INSERT INTO cg105 VALUES {vals};")
        # repeated UPDATE: reuse path (unchanged) + changed path (eager delete)
        for i in range(1, 101):
            q(f"UPDATE cg105 SET b = b WHERE a = {i};")            # reuse
            q(f"UPDATE cg105 SET b = '{big}' WHERE a = {i + 100};")  # changed
        # rollback + upsert mix
        for i in range(1, 21):
            q(f"BEGIN; UPDATE cg105 SET b = '{big}' WHERE a = {i}; ROLLBACK;")
        q("CREATE TABLE cgc(a int PRIMARY KEY, b text);")
        for i in range(1, 51):
            q(f"INSERT INTO cgc VALUES ({i}, '{big}') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b;")
        q("CHECKPOINT;")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()
    print(f"callgrind out: {data_dir}/cg.out")
    print("cg_v105: done (annotate with callgrind_annotate)")

if __name__ == "__main__":
    main()
