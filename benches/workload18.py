#!/usr/bin/env python3
"""Benchmark workload for rustgres v0.18 (numeric type cluster).

Exercises the new numeric features:
- NaN/Infinity parsing and arithmetic
- exp/ln/log (high-precision Taylor series)
- Power with specials
- Special-value comparisons and ordering

Usage: build server, then python3 benches/workload18.py
"""
import socket
import struct
import subprocess
import tempfile
import time
import os
import shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "rustgres")

def cstr(s): return s.encode() + b"\x00"
def msg(t, p): return t + struct.pack("!i", len(p)+4) + p

def run():
    tmp = tempfile.mkdtemp(prefix="rgbench18_")
    proc = subprocess.Popen([BIN, "--port", "5433", "--data-dir", tmp],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.5)
    try:
        s = socket.create_connection(("127.0.0.1", 5433), timeout=10)
        params = cstr("user")+cstr("postgres")+cstr("database")+cstr("postgres")+b"\x00"
        s.sendall(struct.pack("!i", len(params)+8)+struct.pack("!i",196608)+params)
        def read_msg():
            hdr = s.recv(5)
            t, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            p = b""
            while len(p) < ln-4: p += s.recv(ln-4-len(p))
            return t, p
        while True:
            t, _ = read_msg()
            if t == b'Z': break

        queries = [
            "SELECT 'NaN'::numeric + 1",
            "SELECT 'Infinity'::numeric * 2",
            "SELECT exp(1.0)",
            "SELECT ln(4.2::numeric)",
            "SELECT log(4.2::numeric)",
            "SELECT power('Infinity'::numeric, '-2'::numeric)",
            "SELECT sqrt(2)",
            "SELECT 'NaN'::numeric = 'NaN'::numeric",
        ]
        # Warmup
        for q in queries:
            s.sendall(msg(b'Q', cstr(q)))
            while True:
                t, _ = read_msg()
                if t == b'Z': break
        # Benchmark
        start = time.time()
        iterations = 100
        for _ in range(iterations):
            for q in queries:
                s.sendall(msg(b'Q', cstr(q)))
                while True:
                    t, _ = read_msg()
                    if t == b'Z': break
        elapsed = time.time() - start
        total = iterations * len(queries)
        print(f"v0.18 workload: {total} queries in {elapsed:.2f}s ({total/elapsed:.1f} qps)")
        s.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)
        shutil.rmtree(tmp, ignore_errors=True)

if __name__ == "__main__":
    run()
