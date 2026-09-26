#!/usr/bin/env python3
"""dhat_v102.py — DHAT heap profile for the v1.02 RAISE NOTICE /
RAISE EXCEPTION paths (notice sink, %-format rendering, named-arg
rewrite, exception propagation + handlers)."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5599
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat102_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
        q("CREATE TABLE dht2 (x int, y int);")
        q("""CREATE FUNCTION dh_tattle(x int, y int) RETURNS bool AS $$
BEGIN
  RAISE NOTICE 'x = %, y = %', x, y;
  RETURN x > y;
END$$ LANGUAGE plpgsql VOLATILE;""")
        q("""CREATE FUNCTION dh_multi(a int, b text) RETURNS int AS $$
BEGIN
  RAISE NOTICE '100%% of %', a;
  RAISE NOTICE 'pair: %, %', a, b;
  RETURN a;
END$$ LANGUAGE plpgsql VOLATILE;""")
        q("""CREATE FUNCTION dh_boom() RETURNS int AS $$
BEGIN
  RAISE EXCEPTION 'kaput %', 42;
  RETURN 1;
END$$ LANGUAGE plpgsql VOLATILE;""")
        q("""CREATE FUNCTION dh_trap() RETURNS int AS $$
BEGIN
  RAISE NOTICE 'before';
  RAISE EXCEPTION 'nope';
  RETURN 1;
EXCEPTION WHEN raise_exception THEN
  RAISE NOTICE 'caught';
  RETURN -1;
END$$ LANGUAGE plpgsql VOLATILE;""")
        for i in range(20):
            q(f"SELECT dh_tattle({i}, {i % 7});")
            q(f"SELECT dh_multi({i}, 's{i}');")
            q("SELECT dh_trap();")
            q(f"INSERT INTO dht2 VALUES ({i}, dh_multi({i}, 'x')::int);")
            q("SELECT dh_boom();")
        s.close()
        proc.terminate()
        proc.wait(timeout=60)
        print(f"DHAT log: {log}")
        # Report top alloc sites (at-least bytes).
        import re
        txt = open(log).read()
        for pat in [r"Total:\s+([0-9,]+) bytes",
                    r"At t-gmax:\s+([0-9,]+) bytes",
                    r"At t-end:\s+([0-9,]+) bytes in ([0-9,]+) blocks"]:
            m = re.search(pat, txt)
            print(pat.split(":")[0], "->", m.group(0) if m else "n/a")
    finally:
        try:
            proc.kill()
        except Exception:
            pass

if __name__ == "__main__":
    main()
