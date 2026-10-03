#!/usr/bin/env python3
"""dhat_v091.py — DHAT heap profile for v0.91 array_agg + ANY(ARRAY) + Knuth division."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5592
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat91_")
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

        # v0.91 array_agg aggregate paths
        q("select array_agg(i) from generate_series(1,5000) g(i)")
        q("select array_agg(distinct i % 100) from generate_series(1,5000) g(i)")
        q("select array_agg(array[i, i+1]) from generate_series(1,500) g(i)")
        q("select array_agg(i) over () from generate_series(1,500) g(i)")
        # v0.91 op ANY/ALL/SOME(array_expr) paths
        q("select count(*) from generate_series(1,2000) g(i) where i = any(array[1,500,1000,1500,2000])")
        q("select (1,2) = any(array[(1,2),(3,4)]) from generate_series(1,500) g(i)")
        q("select format(string_agg('%s',','), variadic array_agg(i)) from generate_series(1,500) g(i)")
        # v0.90 Knuth div_rem hot paths
        q("select sum(1/i::numeric) from generate_series(1,200) g(i)")
        q("select ln(i::numeric) from generate_series(1,200) g(i)")
        q("select sqrt(i::numeric) from generate_series(1,200) g(i)")


        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=30)
        except subprocess.TimeoutExpired: proc.kill()

    print("dhat_v091: done, log at", log)
    return 0

if __name__ == "__main__":
    sys.exit(main())
