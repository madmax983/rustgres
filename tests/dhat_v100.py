#!/usr/bin/env python3
"""dhat_v100.py — DHAT heap profile for the v1.00 partition + trigger paths
(partition routing, trigger firing, relkind catalog scans)."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5596
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat100_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
        q("CREATE TABLE dhp (a INT, b TEXT) PARTITION BY RANGE (a);")
        for i in range(4):
            lo, hi = i * 100 + 1, (i + 1) * 100 + 1
            q(f"CREATE TABLE dhp{i} PARTITION OF dhp FOR VALUES FROM ({lo}) TO ({hi});")
        q("""CREATE FUNCTION dhtrigf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN new.b := new.b || '!'; RETURN new; END; $$""")
        q("CREATE TRIGGER dhtrig BEFORE INSERT ON dhp0 FOR EACH ROW EXECUTE FUNCTION dhtrigf()")
        for i in range(20):
            q(f"INSERT INTO dhp SELECT g, 'x'||g FROM generate_series({i*25+1}, {(i+1)*25}) g;")
            q("SELECT relname, relkind FROM pg_class WHERE relkind='p';")
            q("SELECT count(*) FROM dhp;")
        s.close()
        proc.terminate()
        proc.wait(timeout=60)
        print(f"DHAT log: {log}")
        # Report top alloc sites (at-least bytes).
        import re
        txt = open(log).read()
        m = re.search(r"Total:\s+([\d,]+) bytes", txt)
        print("dhat total:", m.group(1) if m else "?")
    finally:
        try: proc.kill()
        except Exception: pass

if __name__ == "__main__":
    main()
