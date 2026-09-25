#!/usr/bin/env python3
"""DHAT workload v0.82: 200 repeated extended Parse/Bind/Execute cycles
(parse-cache hits) plus composite text-input casts, for allocation
profiling. Reports total/peak bytes and per-query accumulation."""
import os, socket, struct, subprocess, sys, tempfile, time
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5433
def main():
    data_dir = tempfile.mkdtemp(prefix="rg82dhat_")
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
    def extended(query, param):
        stmt_name = b"stmt1\x00"; qbytes = query.encode() + b"\x00"
        pb = stmt_name + qbytes + struct.pack("!h", 0)
        s.sendall(b"P" + struct.pack("!i", 4 + len(pb)) + pb)
        pval = param.encode()
        bb = b"\x00" + stmt_name + struct.pack("!h", 0) + struct.pack("!h", 1) + struct.pack("!i", len(pval)) + pval + struct.pack("!h", 0)
        s.sendall(b"B" + struct.pack("!i", 4 + len(bb)) + bb)
        s.sendall(b"D" + struct.pack("!i", 6) + b"P" + b"\x00")
        s.sendall(b"E" + struct.pack("!i", 9) + b"\x00" + struct.pack("!i", 0))
        s.sendall(b"S" + struct.pack("!i", 4))
        while True:
            hdr = s.recv(5)
            if len(hdr) < 5: break
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]; s.recv(ln-4)
            if typ == b"Z": break
    simple("CREATE TABLE dh_t (a int, b text)")
    simple("INSERT INTO dh_t SELECT i, 'x' FROM generate_series(1,50) i")
    simple("CREATE TYPE dh_rec AS (x int, y text)")
    query = "SELECT a, b FROM dh_t WHERE a = $1 ORDER BY a"
    for i in range(150):
        extended(query, str(i % 50))
    # Composite text-input casts (v0.82 parse_record_literal path).
    for i in range(50):
        simple("SELECT '(%d,txt%d)'::dh_rec" % (i, i))
    s.sendall(b"X" + struct.pack("!i", 4)); s.close()
    proc.terminate(); proc.wait()
if __name__ == "__main__": main()
