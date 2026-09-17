#!/usr/bin/env python3
"""Valgrind workload: exercise format() paths."""
import socket, struct, subprocess, time, os, sys

PORT = 5547
DATA_DIR = "/tmp/rg45vg"
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c: raise RuntimeError("closed")
        d += c
    return d

def run_sql(s, sql):
    s.sendall(b"Q" + struct.pack("!i", len(sql.encode()) + 5) + sql.encode() + b"\x00")
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"Z":
            break

def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)

    vg_lib = os.path.expanduser("~/workspace/valgrind-local/usr/libexec/valgrind")
    vg_bin = os.path.expanduser("~/workspace/valgrind-local/usr/bin/valgrind")
    env = dict(os.environ, VALGRIND_LIB=vg_lib)

    log = open("/tmp/rg45vg.log", "w")
    proc = subprocess.Popen(
        [vg_bin, "--tool=memcheck", "--error-exitcode=99",
         BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
        stdout=log, stderr=subprocess.STDOUT, env=env)
    time.sleep(8)

    try:
        s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t = read_exact(s, 1)
            ln = struct.unpack("!i", read_exact(s, 4))[0]
            b = read_exact(s, ln - 4)
            if t == b"Z":
                break

        # Exercise format() paths
        stmts = [
            "SELECT format('Hello %s', 'World');",
            "SELECT format('INSERT INTO %I VALUES(%L)', 't', 'a''b');",
            "SELECT format('%1$s %2$s', 'x', 'y');",
            "SELECT format('>>%10s<<', 'hi');",
            "SELECT format('>>%-10s<<', 'hi');",
            "SELECT format('%s', NULL);",
            "SELECT format('%L', NULL);",
            "SELECT format(NULL, 'x');",
            "SELECT format('%s', true);",
            "SELECT format('%%');",
        ]
        for sql in stmts:
            run_sql(s, sql)
        s.close()
        print("workload done")
    finally:
        proc.terminate()
        try:
            rc = proc.wait(timeout=30)
            print(f"valgrind exit: {rc}")
            return 0 if rc == 0 else 1
        except subprocess.TimeoutExpired:
            proc.kill()
            print("valgrind timeout")
            return 1

if __name__ == "__main__":
    sys.exit(main())
