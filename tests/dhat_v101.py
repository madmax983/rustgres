#!/usr/bin/env python3
"""dhat_v101.py — DHAT heap profile for the v1.01 plpgsql EXCEPTION paths
(trapped/untrapped calls, handler matching, named-arg rewrite)."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5597
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat101_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
        q("CREATE TABLE dht (c float8);")
        q("""CREATE FUNCTION dh_inv(int) RETURNS float8 AS $$
BEGIN
  ANALYZE dht;
  RETURN 1::float8/$1;
EXCEPTION
  WHEN division_by_zero THEN RETURN 0;
END$$ LANGUAGE plpgsql VOLATILE;""")
        q("CREATE FUNCTION dh_named(x int) RETURNS int AS 'BEGIN RETURN x + 1; EXCEPTION WHEN OTHERS THEN RETURN 0; END' LANGUAGE plpgsql;")
        for i in range(20):
            q("SELECT dh_inv(0);")
            q(f"SELECT dh_inv({i + 1});")
            q(f"SELECT dh_named({i});")
            q(f"INSERT INTO dht VALUES (dh_inv({i % 3}));")
            q("SELECT dh_named(1/0);")
        s.close()
        proc.terminate()
        proc.wait(timeout=60)
        print(f"DHAT log: {log}")
        # Report top alloc sites (at-least bytes).
        import re
        txt = open(log).read()
        m = re.search(r"Total:\s+([\d,]+) bytes", txt)
        print("dhat total:", m.group(1) if m else "?")
        m2 = re.search(r"At t-gmax:\s+([\d,]+) bytes", txt)
        print("dhat t-gmax:", m2.group(1) if m2 else "?")
    finally:
        try: proc.kill()
        except Exception: pass

if __name__ == "__main__":
    main()
