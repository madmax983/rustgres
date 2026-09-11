#!/usr/bin/env python3
"""v0.17 profiling workload: exercises READ ONLY checks, datetime built-ins,
SET/SHOW, version(), and COPY paths.

Sends N rounds of v0.17-feature statements over the simple protocol so
valgrind (memcheck/callgrind/dhat) can observe the new code paths.
Usage: workload17.py [rounds]  (default 40 -> ~2000 statements)
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
    dd = tempfile.mkdtemp(prefix="rgprof17-")
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
        c.q("CREATE TABLE w17 (a int, b text, d date, t timestamp)")
        c.q("INSERT INTO w17 VALUES (1, 'one', DATE '2026-09-11', TIMESTAMP '2026-09-11 13:00:00')")
        for _ in range(rounds):
            # datetime built-ins
            c.q("SELECT date_part('year', d), date_part('month', d), date_part('dow', d), "
                "date_part('hour', t), to_char(d, 'YYYY/MM/DD'), "
                "to_date('2026-09-11', 'YYYY-MM-DD'), "
                "to_timestamp('2026-09-11 13:00:00', 'YYYY-MM-DD HH24:MI:SS'), "
                "make_date(2026, 9, 11), make_timestamp(2026, 9, 11, 13, 0, 0.0), "
                "timezone('UTC', t), clock_timestamp(), statement_timestamp(), "
                "transaction_timestamp() FROM w17")
            # READ ONLY enforcement (blocked writes)
            c.q("START TRANSACTION READ ONLY")
            c.q("INSERT INTO w17 VALUES (2, 'two', DATE '2026-09-12', TIMESTAMP '2026-09-12 00:00:00')")
            c.q("UPDATE w17 SET b = 'x'")
            c.q("DELETE FROM w17")
            c.q("SELECT * FROM w17")
            c.q("ROLLBACK")
            # SET/SHOW/version
            c.q("SET default_transaction_read_only = on")
            c.q("SHOW default_transaction_read_only")
            c.q("SHOW server_version")
            c.q("SHOW server_version_num")
            c.q("SELECT version()")
            c.q("RESET default_transaction_read_only")
            # COPY TO (read path)
            c.q("COPY w17 TO STDOUT")
        c.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)
    import shutil
    shutil.rmtree(dd, ignore_errors=True)
    print(f"workload done: {rounds} rounds")


if __name__ == "__main__":
    main()
