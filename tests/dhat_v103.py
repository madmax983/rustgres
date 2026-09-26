#!/usr/bin/env python3
"""dhat_v103.py — DHAT heap profile for the v1.03 DECLARE/:=/FOR/RETURN NEXT/SETOF/EXPLAIN ANALYZE paths
(local slots, FOR-query rewriting, SETOF accumulation, EXPLAIN ANALYZE single-execution)."""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5600
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat103_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
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
            qb = sql.encode()
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z": break

        q("CREATE TABLE dh_sq (pk int primary key, c1 int, c2 int);")
        q("INSERT INTO dh_sq SELECT g, g%4, g%4 FROM generate_series(1,50) g;")
        q("""CREATE FUNCTION dh_add(a int, b int) RETURNS int AS $$
declare s int;
begin s := a + b; return s; end;
$$ LANGUAGE plpgsql""")
        q("""CREATE FUNCTION dh_gen(n int) RETURNS SETOF int AS $$
declare i int;
begin
  for i in select * from generate_series(1, n) loop
    return next i;
  end loop;
end;
$$ LANGUAGE plpgsql""")
        q("""CREATE FUNCTION dh_expl() RETURNS SETOF text LANGUAGE plpgsql AS $$
declare ln text;
begin
  for ln in explain (analyze) select * from dh_sq loop
    return next ln;
  end loop;
end;
$$""")
        for i in range(20):
            q(f"SELECT dh_add({i}, {i});")
            q("SELECT * FROM dh_gen(10);")
            q("SELECT * FROM dh_expl();")
            q("EXPLAIN (ANALYZE) SELECT * FROM dh_sq;")
        q("CHECKPOINT;")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait()
    txt = open(log).read()
    print(f"dhat log: {log} ({len(txt)} bytes)")
    # Look for obvious leak growth: total allocated vs freed
    import re
    m = re.search(r"Total:\s+([\d,]+) bytes allocated", txt)
    if m:
        print(f"  total allocated: {m.group(1)}")
    print("dhat_v103: done (inspect log for leak growth vs v102 baseline)")

if __name__ == "__main__":
    main()
