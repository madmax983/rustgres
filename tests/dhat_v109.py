#!/usr/bin/env python3
"""dhat_v109.py — DHAT heap profile for the v1.09 function DDL paths
(function definition allocation, SRF targetlist expansion vectors,
volatility update in place)."""
import os, socket, struct, subprocess, sys, tempfile, time, re

PORT = 5602
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat109_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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

        q("CREATE FUNCTION dh109a(int) RETURNS int LANGUAGE sql IMMUTABLE PARALLEL SAFE AS 'SELECT $1 * 2';")
        q("CREATE FUNCTION dh109srf(int) RETURNS SETOF int AS 'SELECT generate_series(1, $1)' LANGUAGE sql IMMUTABLE;")
        q("CREATE FUNCTION dh109v(int) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT $1';")
        for i in range(20):
            # PARALLEL-hint function definition + call
            q(f"SELECT dh109a({i});")
            # SRF targetlist expansion (vector allocation per row)
            q(f"SELECT dh109srf({i % 10 + 1});")
            q("SELECT dh109srf(5) ORDER BY 1;")
            # ALTER FUNCTION volatility (in-place update)
            q("ALTER FUNCTION dh109v(int) IMMUTABLE;")
            q("ALTER FUNCTION dh109v(int) VOLATILE;")
            q(f"SELECT dh109v({i});")
        q("CHECKPOINT;")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()
    txt = open(log).read()
    print(f"dhat log: {log} ({len(txt)} bytes)")
    m = re.search(r"Total:\s+([\d,]+) bytes allocated", txt)
    if m:
        print(f"  total allocated: {m.group(1)}")

if __name__ == "__main__":
    main()
