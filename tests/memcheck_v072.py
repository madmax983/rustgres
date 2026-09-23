#!/usr/bin/env python3
"""memcheck_v072.py — run the v0.72 new-code paths under valgrind memcheck.

Exercises: LIKE (all option variants), SRF zip/pad/repeat in VALUES and
INSERT, partition routing (RANGE NULL, HASH NULL, childless
intermediate, ATTACH), multi-action ALTER incl. the PARTITION OF crash
shape, WITH (fillfactor), DROP multi-name.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5547
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # LIKE
    "create table m_src (a int not null, b text, c float default 1.5, constraint m_chk check (a > 0))",
    "create table m_like (like m_src)",
    "insert into m_like values (1, 'x')",
    "insert into m_like (a, b) values (2, 'y')",
    "create table m_like2 (like m_src including defaults excluding constraints)",
    "insert into m_like2 values (-5, 'neg')",
    "create table m_like3 (like m_src including constraints)",
    "create table m_like4 (like m_src including all)",
    "create table m_like5 (a int, like m_src including compression)",
    # SRF
    "values (generate_series(1, 3))",
    "values (42, generate_series(1, 2))",
    "values (generate_series(1, 3), generate_series(1, 2))",
    "values (generate_series(5, 4), generate_series(1, 2))",
    "create table m_gs (a int, x int)",
    "insert into m_gs values (7, generate_series(1, 3))",
    "insert into m_gs values (generate_series(5, 4))",
    "select * from m_gs order by x",
    # Partitions
    "create table m_r (a int, b int) partition by range (b)",
    "create table m_r1 partition of m_r for values from (1) to (maxvalue)",
    "create table m_rd partition of m_r default",
    "insert into m_r values (1, null)",
    "create table m_k (a int, b int) partition by range (b)",
    "create table m_k1 partition of m_k for values from (1) to (10) partition by range (a)",
    "create table m_h (a int, b text) partition by hash (a)",
    "create table m_h0 partition of m_h for values with (modulus 4, remainder 0)",
    "insert into m_h values (null, 'n')",
    # Multi-action ALTER + PARTITION OF crash shape
    "create table m_p (a int, b int) partition by range (a)",
    "create table m_c (c text, a int not null, b int not null) partition by list (c)",
    "alter table m_p attach partition m_c for values from (1) to (10)",
    "alter table m_p add d int, add e int",
    "alter table m_p drop e",
    "create table m_cab partition of m_c for values in ('a', 'b') partition by range (c)",
    "create table m_ca partition of m_cab for values from ('a') to ('b')",
    "alter table m_p drop d",
    "insert into m_ca values ('a', 2, 7)",
    # WITH / DROP
    "create table m_ff (a int) with (fillfactor = 80)",
    "create table m_dp (a int) partition by list (a)",
    "create table m_dc partition of m_dp for values in (1)",
    "drop table m_dp, m_dc",
    "checkpoint",
]

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc_")
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leaks=no",
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
        s = socket.create_connection(("127.0.0.1", PORT), timeout=30)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        def rd(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c: raise RuntimeError("closed")
                d += c
            return d
        def msg():
            t = rd(1); ln = struct.unpack("!i", rd(4))[0]
            return t, rd(ln - 4)
        while True:
            t, _ = msg()
            if t == b"Z": break
        nfail = 0
        for q in STMTS:
            qb = q.encode()
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            got_err = None
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i+1]; e = p.find(b"\x00", i + 1)
                        if f == b"C": got_err = p[i+1:e].decode()
                        i = e + 1
                elif t == b"Z":
                    break
            if got_err:
                nfail += 1
                print(f"SQL ERR {got_err}: {q[:60]}")
        s.close()
        print(f"statements with SQL errors: {nfail}")
    finally:
        try:
            proc.terminate(); proc.wait(timeout=30)
        except Exception:
            proc.kill()
    # Analyze valgrind log.
    with open(log) as f:
        txt = f.read()
    errors = 0
    for line in txt.splitlines():
        if "ERROR SUMMARY" in line:
            print(line.strip())
            try:
                errors = int(line.split()[3])
            except Exception:
                pass
    if errors:
        print("--- first 30 error lines ---")
        n = 0
        for line in txt.splitlines():
            if "Invalid " in line or "uninitialised" in line:
                print(line.strip()); n += 1
                if n >= 30: break
        sys.exit(1)
    print("MEMCHECK CLEAN (0 errors)")
    sys.exit(0)

if __name__ == "__main__":
    main()
