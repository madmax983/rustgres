#!/usr/bin/env python3
"""Driver for callgrind: assumes a rustgres server is already running on 5433.
Usage: start server under valgrind first, then run this."""
import socket, struct, time
PORT = 5433
def main():
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
    def extended_cached(query, param):
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
    def extended_distinct(i):
        stmt_name = ("stmt%d" % i).encode() + b"\x00"
        # Note: uses $1 param (0-param Bind has a pre-existing v0.81
        # protocol bug); queries are still distinct SQL text -> cache misses.
        query = ("SELECT a, b FROM cg_t WHERE a = $1 ORDER BY a -- %d" % i).encode() + b"\x00"
        pb = stmt_name + query + struct.pack("!h", 0)
        s.sendall(b"P" + struct.pack("!i", 4 + len(pb)) + pb)
        pval = str(i % 50).encode()
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
    import sys
    mode = sys.argv[1] if len(sys.argv) > 1 else "cached"
    simple("CREATE TABLE cg_t (a int, b text)")
    simple("INSERT INTO cg_t SELECT i, 'x' FROM generate_series(1,50) i")
    if mode == "cached":
        query = "SELECT a, b FROM cg_t WHERE a = $1 ORDER BY a"
        for i in range(200):
            extended_cached(query, str(i % 50))
    else:
        for i in range(200):
            extended_distinct(i)
    s.sendall(b"X" + struct.pack("!i", 4)); s.close()
if __name__ == "__main__": main()
