#!/usr/bin/env python3
"""cg_domain_v085.py — callgrind workload for v0.85 domain paths.

Exercises domain CHECK enforcement (the hot path: check_domain_value per
row) with a bulk INSERT workload, plus temp-table VACUUM.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5586
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgcg85_")
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

        # setup: domain with CHECK
        q("create domain cg_pos as int check (value > 0)")
        q("create table cg_t (a cg_pos, b int)")
        # bulk insert: 2000 rows through domain enforcement
        vals = ",".join(f"({i},{i})" for i in range(1, 2001))
        q(f"insert into cg_t values {vals}")
        # select them back
        q("select count(*) from cg_t")
        q("select sum(a) from cg_t")
        # temp vacuum
        q("create temp table cg_tmp (a int)")
        q(f"insert into cg_tmp values {vals}")
        q("vacuum (analyze) cg_tmp")

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=30)
        except subprocess.TimeoutExpired: proc.kill()

    print("cg_domain_v085: done, output at", os.path.join(data_dir, "cg.out"))
    return 0

if __name__ == "__main__":
    sys.exit(main())
