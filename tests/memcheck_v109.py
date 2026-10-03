#!/usr/bin/env python3
"""memcheck_v109.py — valgrind memcheck for the v1.09 function DDL paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; CREATE FUNCTION with PARALLEL
           options, user SRF targetlist expansion, ALTER FUNCTION
           volatility changes, plus error paths.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           functions persist; more traffic.

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5597

WORKLOAD = [
    # PARALLEL option variants
    ("ddl", "CREATE OR REPLACE FUNCTION mc109a(int) RETURNS int LANGUAGE sql IMMUTABLE PARALLEL SAFE AS 'SELECT $1 * 2';", None),
    ("ddl", "CREATE OR REPLACE FUNCTION mc109b(int) RETURNS int LANGUAGE sql PARALLEL RESTRICTED AS 'SELECT $1';", None),
    ("ddl", "CREATE OR REPLACE FUNCTION mc109c(int) RETURNS int LANGUAGE sql PARALLEL UNSAFE AS 'SELECT $1';", None),
    ("ddl", "CREATE OR REPLACE FUNCTION mc109d(int) RETURNS int LANGUAGE sql PARALLEL BOGUS AS 'SELECT $1';", "42601"),
    ("q", "SELECT mc109a(21);", None),
    # User SRF in targetlist
    ("ddl", "CREATE OR REPLACE FUNCTION mc109srf(int) RETURNS SETOF int AS 'values (1),(10),(2),($1)' LANGUAGE sql IMMUTABLE;", None),
    ("q", "SELECT mc109srf(5);", None),
    ("q", "SELECT mc109srf(-1) ORDER BY 1;", None),
    ("q", "SELECT x, mc109srf(2) FROM generate_series(1,3) g(x);", None),
    ("ddl", "DROP FUNCTION mc109srf(int);", None),
    # ALTER FUNCTION volatility
    ("ddl", "CREATE OR REPLACE FUNCTION mc109v(int) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT $1';", None),
    ("ddl", "ALTER FUNCTION mc109v(int) IMMUTABLE;", None),
    ("ddl", "ALTER FUNCTION mc109v(int) STABLE;", None),
    ("ddl", "ALTER FUNCTION mc109v(int) VOLATILE;", None),
    ("ddl", "ALTER FUNCTION mc109v(int) STRICT;", "0A000"),
    ("ddl", "ALTER FUNCTION nosuchfn(int) IMMUTABLE;", "42883"),
    ("q", "SELECT mc109v(7);", None),
    # plpgsql with PARALLEL
    ("ddl", "CREATE OR REPLACE FUNCTION mc109p(int) RETURNS int AS $$ BEGIN RETURN $1 + 1; END; $$ LANGUAGE plpgsql PARALLEL SAFE;", None),
    ("q", "SELECT mc109p(41);", None),
]


def run_workload(simple, expect_ok, expect_err):
    for name, sql, want in WORKLOAD:
        codes = simple(sql)
        if want is None:
            expect_ok(name, codes, sql)
        else:
            expect_err(name, codes, want, sql)


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc109_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log1 = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log1}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    nfail = [0]
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

        def expect_ok(name, codes, sql):
            if codes:
                print(f"UNEXPECTED ERR {name}: {codes} :: {sql[:60]}")
                nfail[0] += 1

        def expect_err(name, codes, want, sql):
            if codes != [want]:
                print(f"WRONG ERR {name}: got {codes} want [{want}] :: {sql[:60]}")
                nfail[0] += 1

        print("phase 1: fresh datadir", flush=True)
        run_workload(simple, expect_ok, expect_err)
        s.close()
        proc.terminate()
        proc.wait(timeout=120)

        # phase 2: WAL replay
        log2 = os.path.join(data_dir, "vg2.log")
        proc = subprocess.Popen(
            [VG, "--tool=memcheck", "--error-exitcode=99",
             "--errors-for-leak-kinds=none",
             f"--log-file={log2}",
             BIN, "--data-dir", data_dir, "--port", str(PORT)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not restart under valgrind"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=60)
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = msg()
            if t == b"Z":
                break
        print("phase 2: WAL replay", flush=True)
        run_workload(simple, expect_ok, expect_err)
        # functions persisted across restart
        codes = simple("SELECT mc109a(10);")
        expect_ok("persisted fn", codes, "SELECT mc109a(10)")
        s.close()
        proc.terminate()
        rc = proc.wait(timeout=120)
        print(f"valgrind exit: {rc}, workload failures: {nfail[0]}")
        for log in (log1, log2):
            if os.path.exists(log):
                with open(log) as f:
                    txt = f.read()
                errs = txt.count("ERROR SUMMARY")
                print(f"{os.path.basename(log)}: {errs} summaries")
        sys.exit(1 if nfail[0] else 0)
    finally:
        try:
            proc.kill()
        except Exception:
            pass

if __name__ == "__main__":
    main()
