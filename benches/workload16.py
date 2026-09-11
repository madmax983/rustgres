#!/usr/bin/env python3
"""v0.16 profiling workload: exercises new built-ins, cursors, TRUNCATE.

Sends N rounds of v0.16-feature statements over the simple protocol so
valgrind (memcheck/callgrind/dhat) can observe the new code paths.
Usage: workload16.py [rounds]  (default 40 -> ~2000 statements)
"""
import os, socket, struct, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "rustgres")
HOST, PORT = "127.0.0.1", 5433


def msg(typ, payload):
    return typ + struct.pack("!i", len(payload) + 4) + payload


def rd(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("closed")
        d += c
    return d


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=60)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t = self.s.recv(1)
            (ln,) = struct.unpack("!i", rd(self.s, 4))
            p = rd(self.s, ln - 4)
            if t == b"Z":
                return

    def q(self, sql):
        self.s.sendall(msg(b"Q", sql.encode() + b"\x00"))
        while True:
            t = self.s.recv(1)
            (ln,) = struct.unpack("!i", rd(self.s, 4))
            rd(self.s, ln - 4)
            if t == b"Z":
                return

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        except OSError:
            pass
        self.s.close()


def main():
    rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    dd = tempfile.mkdtemp(prefix="rgprof16-")
    proc = subprocess.Popen([BIN, "--port", str(PORT), "--data-dir", dd],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                socket.create_connection((HOST, PORT), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        c = Conn()
        c.q("CREATE TABLE w16 (a int, b text)")
        for _ in range(rounds):
            c.q("SELECT substr('hello world', 2, 5), concat('a', NULL, 1), "
                "concat_ws(',', 'x', NULL, 'y'), to_hex(-1234), "
                "to_hex(-1234::bigint), to_oct(255), to_bin(-7), "
                "sign(-3), sign(0.0), left('abcdef', -2), right('abcdef', 2), "
                "reverse('héllo')")
            c.q("TRUNCATE w16")
            c.q("INSERT INTO w16 VALUES (1, 'one'), (2, 'two'), (3, 'three')")
            c.q("BEGIN")
            c.q("DECLARE wc CURSOR FOR SELECT a, b FROM w16 ORDER BY a")
            c.q("FETCH NEXT FROM wc")
            c.q("FETCH 2 FROM wc")
            c.q("FETCH ALL FROM wc")
            c.q("MOVE BACKWARD 2 FROM wc")
            c.q("FETCH RELATIVE 1 FROM wc")
            c.q("CLOSE wc")
            c.q("COMMIT")
        c.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)
    import shutil
    shutil.rmtree(dd, ignore_errors=True)
    print(f"workload done: {rounds} rounds")


if __name__ == "__main__":
    main()
