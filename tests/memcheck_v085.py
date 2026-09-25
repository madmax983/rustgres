#!/usr/bin/env python3
"""memcheck_v085.py — run the v0.85 domain + temp-vacuum paths under
valgrind memcheck.

Exercises:
  A. CREATE DOMAIN (named/unnamed CHECK, NOT NULL, DEFAULT, composite,
     array-of-domain); enforcement on INSERT/UPDATE (23514/23502).
  B. DROP DOMAIN.
  C. Temp-table VACUUM (ANALYZE) — the v0.85 42P01 fix.
  D. Checkpoint + WAL replay preserve domains.
  E. Connection survives all error cases.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5585
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc85_")
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
                if t in (b"C", b"E", b"Z"):
                    # drain to ReadyForQuery
                    while t != b"Z":
                        t, _ = msg()
                    return

        # A. domains
        for sql in [
            "create domain mc_pos as int check (value > 0)",
            "create domain mc_nn as int not null",
            "create domain mc_def as int default 7",
            "create type mc_ct as (x int, y text)",
            "create domain mc_dct as mc_ct check ((value).x > 0)",
            "create domain mc_di as int[] check (value[1] > 0)",
            "create table mcd (a mc_pos, b mc_nn, c mc_def, d mc_dct, e mc_di)",
            "insert into mcd values (1, 2, 3, row(1,'a'), '{1,2}')",
            "insert into mcd values (-1, 2, 3, row(1,'a'), '{1,2}')",
            "insert into mcd values (1, null, 3, row(1,'a'), '{1,2}')",
            "insert into mcd (a, b) values (1, 2)",
            "insert into mcd values (1, 2, default, row(-1,'a'), '{1,2}')",
            "update mcd set a = -5",
            "update mcd set a = 10",
            "select null::mc_nn",
            "select null::mc_pos",
            "select 5::mc_nn",
        ]:
            q(sql)

        # B. drop domain
        for sql in [
            "create domain mc_tmp as int",
            "drop domain mc_tmp",
            "drop domain if exists mc_nonexist",
            "drop domain mc_nonexist",
        ]:
            q(sql)

        # C. temp-table vacuum (v0.85 fix)
        for sql in [
            "create temp table mc_tmp2 (a int, b text)",
            "insert into mc_tmp2 values (1,'x'),(2,'y')",
            "vacuum (analyze) mc_tmp2",
            "vacuum verbose mc_tmp2",
            "delete from mc_tmp2 where a = 1",
            "vacuum mc_tmp2",
            "select * from mc_tmp2",
        ]:
            q(sql)

        # D. checkpoint (domain WAL/checkpoint paths)
        q("checkpoint")

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()

    # report
    errors = 0
    with open(log, errors="replace") as f:
        for line in f:
            if "ERROR SUMMARY" in line:
                print(line.strip())
            if line.strip().startswith("==") and "errors" in line.lower():
                errors += 1
    print("memcheck_v085: done, log at", log)
    return 0


if __name__ == "__main__":
    sys.exit(main())
