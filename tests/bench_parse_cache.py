#!/usr/bin/env python3
"""v0.81 parse-cache benchmark: repeated extended-protocol Parse of the same query.

Uses extended protocol (Parse/Bind/Execute) to measure parse-cache benefit.
First cycle misses; subsequent cycles hit.
"""
import os, socket, struct, subprocess, sys, tempfile, time

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5433

def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", data_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.terminate()
    raise RuntimeError("server did not start")

def main():
    data_dir = tempfile.mkdtemp(prefix="rg81bench_")
    proc = start_server(data_dir)
    try:
        s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
        while True:
            hdr = s.recv(5)
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            s.recv(ln - 4)
            if typ == b"Z": break

        def simple(sql):
            q = b"Q" + struct.pack("!i", 4 + len(sql) + 1) + sql.encode() + b"\x00"
            s.sendall(q)
            while True:
                hdr = s.recv(5)
                typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
                s.recv(ln - 4)
                if typ == b"Z": break

        def extended(query, param):
            # Parse (named stmt1)
            stmt_name = b"stmt1\x00"
            qbytes = query.encode() + b"\x00"
            parse_body = stmt_name + qbytes + struct.pack("!h", 0)
            s.sendall(b"P" + struct.pack("!i", 4 + len(parse_body)) + parse_body)
            # Bind: portal="", stmt="stmt1", 0 param formats, 1 param, 0 result formats
            pval = param.encode()
            bind_body = b"\x00" + stmt_name + struct.pack("!h", 0) + struct.pack("!h", 1) + struct.pack("!i", len(pval)) + pval + struct.pack("!h", 0)
            s.sendall(b"B" + struct.pack("!i", 4 + len(bind_body)) + bind_body)
            # Describe portal, Execute, Sync
            s.sendall(b"D" + struct.pack("!i", 6) + b"P" + b"\x00")
            s.sendall(b"E" + struct.pack("!i", 9) + b"\x00" + struct.pack("!i", 0))
            s.sendall(b"S" + struct.pack("!i", 4))
            while True:
                hdr = s.recv(5)
                if len(hdr) < 5: break
                typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
                body = s.recv(ln - 4)
                if typ == b"Z": break
                if typ == b"E":
                    print(f"  extended error: {body[:80]}")
                    break

        simple("CREATE TABLE bench_t (a int, b text)")
        simple("INSERT INTO bench_t SELECT i, 'x' FROM generate_series(1,100) i")

        query = "SELECT a, b FROM bench_t WHERE a = $1 ORDER BY a"
        n = 1000
        extended(query, "42")  # warmup (miss)

        start = time.perf_counter()
        for i in range(n):
            extended(query, str(i % 100))
        elapsed = time.perf_counter() - start
        print(f"parse_cache_bench: {n} extended Parse/Bind/Execute cycles in {elapsed:.3f}s")
        print(f"  {elapsed/n*1000:.3f} ms/cycle")
        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        proc.wait()

if __name__ == "__main__":
    main()
