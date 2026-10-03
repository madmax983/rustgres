#!/usr/bin/env python3
"""dhat_v099.py — DHAT heap profile for v0.98 sequence paths."""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5599
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat99_")
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
            s.sendall(b"Q" + struct.pack("!i", 5 + len(sql)) + sql.encode() + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z": return

        # v0.99 paths (typed-sequence DDL, catalog scans, correlated
        # derived tables, parse_ident table function)
        q("create table dh99t(q1 int, q2 int)")
        q("insert into dh99t select g, g from generate_series(1, 2000) g")
        for i in range(20):
            tp = ("smallint", "int", "bigint")[i % 3]
            q(f"create sequence dh99s{i} as {tp}")
            q(f"alter sequence dh99s{i} as int")
            q(f"select nextval('dh99s{i}')")
            q(f"drop sequence dh99s{i}")
        for _ in range(20):
            q("select * from pg_sequences")
            q("select sequence_name, cycle_option from information_schema.sequences")
        for _ in range(20):
            q("select *, (select r from (select q1 as q2) x, (select q2 as r) y) from dh99t")
        for _ in range(20):
            q("select * from parse_ident('\"Test\".col')")
        q("checkpoint")

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=30)
        except subprocess.TimeoutExpired: proc.kill()

    print("dhat_v099: done, log at", log)
    try:
        with open(log) as f:
            txt = f.read()
        for pat in (r"Total:\s+([0-9,]+ bytes[^\n]*)",
                    r"Maximum live:[^\n]*",
                    r"At end of run[^\n]*"):
            m = re.search(pat, txt)
            if m:
                print("DHAT:", m.group(0).strip()[:160])
    except Exception as e:
        print("dhat log parse failed:", e)
    return 0

if __name__ == "__main__":
    sys.exit(main())
