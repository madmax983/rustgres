#!/usr/bin/env python3
"""dhat_v098.py — DHAT heap profile for v0.98 sequence paths."""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5598
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat98_")
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

        # v0.98 sequence paths (heap-heavy: per-row Value allocs in the
        # volatile WHERE, seq state HashMaps, catalog row construction)
        q("create table dh98t as select g % 100 as v from generate_series(1, 5000) g")
        q("create sequence dh98s")
        q("create sequence dh98c cache 100")
        for _ in range(20):
            q("select count(*) from (select distinct v from dh98t) ss where v < 100 + nextval('dh98s')")
        for _ in range(20):
            q("select nextval('dh98s'), currval('dh98s'), lastval()")
            q("select setval('dh98s', 1000, false)")
        for _ in range(20):
            q("select * from pg_sequences")
            q("select * from information_schema.sequences")
        for i in range(20):
            q(f"create sequence dh98x{i} cache {1 + i % 50}")
            q(f"alter sequence dh98x{i} restart with {i + 1}")
            q(f"drop sequence dh98x{i}")
        q("checkpoint")

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=30)
        except subprocess.TimeoutExpired: proc.kill()

    print("dhat_v098: done, log at", log)
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
