#!/usr/bin/env python3
"""memcheck_v104.py — valgrind memcheck for the v1.04 multi-statement
simple-Query implicit-transaction paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; multi-statement Q traffic:
           ordered repeated result sets, mixed SELECT/DDL/DML,
           mid-Q error skipping the rest + whole-Q rollback,
           COMMIT/ROLLBACK-in-block warnings with fresh blocks,
           25001 rejections (VACUUM/SAVEPOINT/ROLLBACK TO/RELEASE/
           AND CHAIN), BEGIN conversion to explicit txn, CHECKPOINT.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           a table committed via a multi-statement Q is still there;
           more multi-statement traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5599

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc104_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log1 = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log1}",
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
        s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
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

        def simple(q):
            qb = q.encode()
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            codes = []
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            codes.append(p[i + 1:e].decode())
                        i = e + 1
                elif t == b"Z":
                    break
            return codes

        nfail = 0
        def expect_ok(name, codes):
            nonlocal nfail
            if codes:
                nfail += 1
                print(f"SQL ERR {codes}: {name[:70]}")

        def expect_err(name, codes, want):
            nonlocal nfail
            if codes != [want]:
                nfail += 1
                print(f"SQL ERR {codes} (want [{want}]): {name[:70]}")

        # ---- A: ordered repeated result sets ----
        expect_ok("3 selects", simple("SELECT 1; SELECT 2; SELECT 3;"))
        expect_ok("2 selects", simple("SELECT 'a'; SELECT 'b';"))

        # ---- B: mixed SELECT/DDL/DML, one implicit block ----
        expect_ok("mixed ddl/dml", simple(
            "CREATE TABLE mc104(a int); INSERT INTO mc104 VALUES (1),(2); "
            "SELECT * FROM mc104 ORDER BY 1;"))

        # ---- C: mid-Q error skips rest, whole Q rolls back ----
        expect_err("mid-q div0", simple(
            "CREATE TABLE mc_rb(a int); INSERT INTO mc_rb VALUES (1); SELECT 1/0;"),
            "22012")
        expect_err("rb table gone", simple("SELECT * FROM mc_rb;"), "42P01")

        # ---- D: COMMIT/ROLLBACK-in-block warn; fresh block follows ----
        expect_ok("commit in block", simple("SELECT 1; COMMIT; SELECT 2;"))
        expect_ok("rollback in block", simple(
            "CREATE TABLE mc_rb2(a int); INSERT INTO mc_rb2 VALUES (9); "
            "ROLLBACK; SELECT 1;"))
        expect_err("rb2 table gone", simple("SELECT * FROM mc_rb2;"), "42P01")

        # ---- E: 25001 rejections inside the block ----
        expect_err("vacuum", simple("SELECT 1; VACUUM;"), "25001")
        expect_err("savepoint", simple("SELECT 1; SAVEPOINT sp;"), "25001")
        expect_err("rollback to", simple("SELECT 1; ROLLBACK TO sp;"), "25001")
        expect_err("release", simple("SELECT 1; RELEASE sp;"), "25001")
        expect_err("commit chain", simple("SELECT 1; COMMIT AND CHAIN;"), "25001")
        expect_err("rollback chain", simple("SELECT 1; ROLLBACK AND CHAIN;"), "25001")

        # ---- F: BEGIN converts; explicit txn survives the Q ----
        expect_ok("begin converts", simple("SELECT 1; BEGIN; SELECT 2;"))
        expect_ok("commit ends it", simple("COMMIT;"))

        # ---- G: single-statement autocommit regression ----
        expect_ok("single select", simple("SELECT 42;"))
        expect_ok("single insert", simple(
            "CREATE TABLE mc1(a int); INSERT INTO mc1 VALUES (5);"))
        expect_ok("durable read", simple("SELECT * FROM mc1;"))

        expect_ok("checkpoint", simple("CHECKPOINT"))
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    # Phase 2: restart under valgrind
    log2 = os.path.join(data_dir, "vg2.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log2}",
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
            print("phase2: server did not start"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        def rd2(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d
        def msg2():
            t = rd2(1); ln = struct.unpack("!i", rd2(4))[0]
            return t, rd2(ln - 4)
        while True:
            t, _ = msg2()
            if t == b"Z":
                break
        def simple2(q):
            qb = q.encode()
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            got_err = None
            while True:
                t, p = msg2()
                if t == b"E":
                    got_err = "E"
                elif t == b"Z":
                    break
            return got_err
        # mc1 was committed via multi-statement Qs: WAL replay must keep it.
        if simple2("SELECT * FROM mc1;"):
            nfail += 1; print("phase2: mc1 missing after replay")
        if simple2("SELECT 1; SELECT 2; SELECT 3;"):
            nfail += 1; print("phase2: multi-select failed")
        if simple2("CREATE TABLE mc_p2(a int); INSERT INTO mc_p2 VALUES (8); SELECT * FROM mc_p2;"):
            nfail += 1; print("phase2: multi-ddl/dml failed")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    for log in (log1, log2):
        txt = open(log).read()
        print(f"{os.path.basename(log)}: {'FAIL' if nfail else 'OK'} sql_errors={nfail}")
        # Count valgrind errors
        import re
        m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
        if m:
            print(f"  valgrind errors: {m.group(1)}")
            if m.group(1) != "0":
                sys.exit(1)
    sys.exit(1 if nfail else 0)

if __name__ == "__main__":
    main()
