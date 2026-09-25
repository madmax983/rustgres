#!/usr/bin/env python3
"""dhat_v094.py — DHAT heap profile for the v0.94 hash-join / grouping
canonicalization paths (Numeric::hash_key BigUint allocs, canon_float_key,
pg_float_ord, value_key_numeric)."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5595
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat94_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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

        # v0.94 hash-join canonicalization (heap-heavy: BigUint per key)
        q("create table dhi(id int); create table dhi2(id int);")
        q("insert into dhi select g from generate_series(1,3000) g;")
        q("insert into dhi2 select g from generate_series(1,3000) g;")
        q("select count(*) from dhi a join dhi2 b on a.id = b.id;")
        q("create table dhn(id numeric); create table dhn2(id numeric);")
        q("insert into dhn select (g || '.00')::numeric from generate_series(1,3000) g;")
        q("insert into dhn2 select g::numeric from generate_series(1,3000) g;")
        q("select count(*) from dhn a join dhn2 b on a.id = b.id;")
        q("create table dhf(id float8); create table dhf2(id float8);")
        q("insert into dhf select case when g % 700 = 0 then 'NaN'::float8 when g % 500 = 0 then -0.0 else g::float8 end from generate_series(1,3000) g;")
        q("insert into dhf2 select case when g % 700 = 0 then 'NaN'::float8 when g % 500 = 0 then 0.0 else g::float8 end from generate_series(1,3000) g;")
        q("select count(*) from dhf a join dhf2 b on a.id = b.id;")
        q("select count(*) from (select distinct id from dhn) t;")
        q("select count(*) from (select distinct id from dhf) t;")
        q("select id from dhf order by id limit 10;")

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=30)
        except subprocess.TimeoutExpired: proc.kill()

    print("dhat_v094: done, log at", log)
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
