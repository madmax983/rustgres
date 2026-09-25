#!/usr/bin/env python3
"""memcheck_v084.py — run the v0.84 INSERT target indirection paths under
valgrind memcheck.

Exercises:
  A. Canonical indirection: f2[1],f2[2] (VALUES + multi-row + SELECT),
     f3.if1/f3.if2, f3.if2[1]/[2], f4[1].if2[1]/[2].
  B. DEFAULT into indirection -> 0A000; whole+partial -> 42701.
  C. WAL Record tag 18: composite arrays persist; select after insert.
  D. Connection survives all error cases.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5584
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc84_")
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
            s.sendall(b"Q" + struct.pack("!i", len(sql) + 5) + sql.encode() + b"\x00")
            out = []
            while True:
                t, b = msg()
                out.append((t, b))
                if t == b"Z":
                    break
            return out

        def errcode(msgs):
            for t, b in msgs:
                if t == b"E":
                    i = 0
                    while i < len(b) - 1:
                        f = b[i:i + 1]
                        e = b.find(b"\x00", i + 1)
                        if e < 0:
                            break
                        if f == b"C":
                            return b[i + 1:e].decode()
                        i = e + 1
            return None

        sem_fails = 0
        def check(name, sql, want):
            nonlocal sem_fails
            got = errcode(q(sql))
            if got != want:
                sem_fails += 1
                print(f"SEM-FAIL {name}: want={want} got={got}")

        for sql in ["create type insert_test_type as (if1 int, if2 text[])",
                    "create table inserttest (f1 int, f2 int[], f3 insert_test_type, f4 insert_test_type[])"]:
            check("setup", sql, None)
        check("arr1", "insert into inserttest (f2[1], f2[2]) values (1,2)", None)
        check("arr2", "insert into inserttest (f2[1], f2[2]) values (3,4), (5,6)", None)
        check("arr3", "insert into inserttest (f2[1], f2[2]) select 7,8", None)
        check("comp1", "insert into inserttest (f3.if1, f3.if2) values (1, '{foo}')", None)
        check("comp2", "insert into inserttest (f3.if2[1], f3.if2[2]) values ('baz','quux')", None)
        check("deep", "insert into inserttest (f4[1].if2[1], f4[1].if2[2]) values ('a','b')", None)
        check("def1", "insert into inserttest (f2[1]) values (default)", "0A000")
        check("def2", "insert into inserttest (f3.if1) values (default)", "0A000")
        check("dup", "insert into inserttest (f2, f2[1]) values ('{9}', 9)", "42701")
        check("sel1", "select f2 from inserttest", None)
        check("sel2", "select f3, f4 from inserttest", None)
        check("alive", "select 1", None)

        s.sendall(b"X")
        s.close()
        time.sleep(2)
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
        print(f"semantic failures: {sem_fails}")
        with open(log) as f:
            txt = f.read()
        import re
        m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
        errs = int(m.group(1)) if m else -1
        print(f"valgrind errors: {errs}")
        for pat in ["definitely lost", "indirectly lost"]:
            m = re.search(pat + r": ([\d,]+) bytes", txt)
            print(f"{pat}: {m.group(1) if m else '?'}")
        sys.exit(1 if (sem_fails or errs != 0) else 0)
    finally:
        try:
            proc.kill()
        except Exception:
            pass

main()
