#!/usr/bin/env python3
"""Callgrind workload v0.82: 200 extended Parse/Bind/Execute cycles, each a
DISTINCT query (parse-cache misses / cold parsing). Pair with
cg_parse_cache.py (200 repeats = cache hits) for cold-vs-cached Ir/query."""
import os, socket, struct, subprocess, sys, tempfile, time
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5433
def main():
    data_dir = tempfile.mkdtemp(prefix="rg82cgcold_")
    proc = subprocess.Popen([BIN, "--port", str(PORT), "--data-dir", data_dir],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1); s.close(); break
        except OSError: time.sleep(0.1)
    s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
    params = b"user\x00postgres\x00database\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
    while True:
        hdr = s.recv(5); typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]; s.recv(ln-4)
        if typ == b"Z": break
    def simple(sql):
        s.sendall(b"Q" + struct.pack("!i", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        while True:
            hdr = s.recv(5); typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]; s.recv(ln-4)
            if typ == b"Z": break
    def extended_distinct(i):
        stmt_name = ("stmt%d" % i).encode() + b"\x00"
        query = ("SELECT a, b FROM cg_t WHERE a = %d ORDER BY a" % (i % 50)).encode() + b"\x00"
        pb = stmt_name + query + struct.pack("!h", 0)
        s.sendall(b"P" + struct.pack("!i", 4 + len(pb)) + pb)
        bb = b"\x00" + stmt_name + struct.pack("!h", 0) + struct.pack("!h", 0)
        s.sendall(b"B" + struct.pack("!i", 4 + len(bb)) + bb)
        s.sendall(b"D" + struct.pack("!i", 6) + b"P" + b"\x00")
        s.sendall(b"E" + struct.pack("!i", 9) + b"\x00" + struct.pack("!i", 0))
        s.sendall(b"S" + struct.pack("!i", 4))
        while True:
            hdr = s.recv(5)
            if len(hdr) < 5: break
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]; s.recv(ln-4)
            if typ == b"Z": break
    simple("CREATE TABLE cg_t (a int, b text)")
    simple("INSERT INTO cg_t SELECT i, 'x' FROM generate_series(1,50) i")
    for i in range(200):
        extended_distinct(i)
    s.sendall(b"X" + struct.pack("!i", 4)); s.close()
    proc.terminate(); proc.wait()
if __name__ == "__main__": main()
