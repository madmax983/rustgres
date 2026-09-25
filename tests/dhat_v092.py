#!/usr/bin/env python3
"""dhat_v092.py — DHAT heap profile for v0.92 array_agg + ORDER BY-in-aggregate."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5594
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat92_")
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

        # v0.92 array_agg NULL-keeping paths (heap-heavy: per-row Value allocs)
        q("select array_agg(i) from generate_series(1,5000) g(i)")
        q("select array_agg(case when i % 3 = 0 then null else i end) from generate_series(1,5000) g(i)")
        q("select array_agg(distinct i % 100) from generate_series(1,5000) g(i)")
        q("select array_agg(array[i, i+1]) from generate_series(1,500) g(i)")
        q("select array_agg(i) over () from generate_series(1,500) g(i)")
        # v0.92 ORDER BY in aggregates (sort-key vecs + row reorder)
        q("select array_agg(i order by i desc) from generate_series(1,5000) g(i)")
        q("select array_agg(i order by i % 100, i) from generate_series(1,5000) g(i)")
        q("select string_agg(i::text, ',' order by i desc) from generate_series(1,2000) g(i)")
        q("select sum(i order by i) from generate_series(1,5000) g(i)")
        q("select i % 10, array_agg(i order by i desc) from generate_series(1,2000) g(i) group by 1")
        # v0.90 Knuth div_rem hot paths (BigUint arithmetic)
        q("select sum(1/i::numeric) from generate_series(1,200) g(i)")
        q("select ln(i::numeric) from generate_series(1,200) g(i)")
        q("select sqrt(i::numeric) from generate_series(1,200) g(i)")


        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=30)
        except subprocess.TimeoutExpired: proc.kill()

    print("dhat_v092: done, log at", log)
    # Print the headline numbers
    try:
        with open(log) as f:
            txt = f.read()
        import re
        for pat in (r"Total:\s+([0-9,]+ bytes[^\n]*)",
                    r"At 0x[^:]*: ([^\n]*max-live[^\n]*)",
                    r"Maximum live:[^\n]*",
                    r"At end of run[^\n]*"):
            m = re.search(pat, txt)
            if m:
                print("DHAT:", m.group(0).strip()[:120])
    except Exception as e:
        print("dhat log parse failed:", e)
    return 0

if __name__ == "__main__":
    sys.exit(main())
