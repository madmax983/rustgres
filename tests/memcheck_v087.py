#!/usr/bin/env python3
"""memcheck_v087.py — run the v0.87 function overload + temp index paths
under valgrind memcheck.

Exercises:
  A. CREATE FUNCTION overloads (two signatures), dispatch, DROP one.
  B. DROP FUNCTION CASCADE/RESTRICT with operator dependencies (2BP01).
  C. Temp-table CREATE INDEX / DROP INDEX.
  D. Zero-target-list SELECT.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5589
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc87_")
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}",
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
            print("server did not start under valgrind"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d

        def msg():
            t = rd(1)
            ln = struct.unpack("!i", rd(4))[0]
            return t, rd(ln - 4)

        while True:
            t, _ = msg()
            if t == b"Z":
                break

        def q(sql):
            s.sendall(b"Q" + struct.pack("!i", 5 + len(sql)) + sql.encode() + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z":
                    return

        # A. overloads
        for sql in [
            "create function mc_add(int, int) returns int as $$ select $1 + $2 $$ language sql",
            "create function mc_add(text, text) returns text as $$ select $1 || $2 $$ language sql",
            "select mc_add(1, 2)",
            "select mc_add('a', 'b')",
            "drop function mc_add(int, int)",
            "select mc_add('x', 'y')",
            "drop function mc_add(text, text)",
        ]:
            q(sql)

        # B. drop cascade/restrict
        for sql in [
            "create function mc_eq(int, int) returns bool as $$ select $1 = $2 $$ language sql",
            "create operator ?= (procedure = mc_eq, leftarg = int, rightarg = int)",
            "drop function mc_eq(int, int)",
            "drop function mc_eq(int, int) cascade",
            "drop operator ?= (int, int)",
        ]:
            q(sql)

        # C. temp indexes
        for sql in [
            "create temp table mc_t (a int, b text)",
            "create index mc_i on mc_t (a)",
            "insert into mc_t values (1,'x'),(2,'y')",
            "select * from mc_t where a = 1",
            "drop index mc_i",
            "select * from mc_t",
        ]:
            q(sql)

        # D. zero-target select
        for sql in [
            "select where true",
            "select where false",
            "create table mc_z (a int)",
            "select where exists (select 1 from mc_z)",
        ]:
            q(sql)

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()

    errors = 0
    with open(log, errors="replace") as f:
        for line in f:
            if "ERROR SUMMARY" in line:
                print(line.strip())
    print("memcheck_v087: done, log at", log)
    return 0


if __name__ == "__main__":
    sys.exit(main())
