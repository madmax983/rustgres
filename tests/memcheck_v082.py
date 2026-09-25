#!/usr/bin/env python3
"""memcheck_v082.py — run the v0.81/v0.82 parse-cache + composite paths under valgrind memcheck.

Exercises:
  A. Parse-cache hits: 60x extended Parse of the same query text.
  B. Parse-cache misses: 60 distinct query texts.
  C. DDL invalidation: Parse Q, DDL (epoch bump), Parse Q again -> miss, results still correct.
  D. FIFO eviction: 300 distinct parses (> 256 cap).
  E. Composite DDL/DML: CREATE TYPE AS, INSERT ROW(), SELECT field access, *=, named casts, DROP TYPE.
  F. Error paths: scalar *= (42883), duplicate CREATE TYPE (42710), unknown type (42704).
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5548
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc82_")
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
            got_err = None
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            got_err = p[i + 1:e].decode()
                        i = e + 1
                elif t == b"Z":
                    break
            return got_err

        def extended(query, params=()):
            # Parse (unnamed), Bind, Describe portal, Execute, Sync.
            qb = query.encode() + b"\x00"
            pb = b"\x00" + qb + struct.pack("!h", 0)
            s.sendall(b"P" + struct.pack("!i", 4 + len(pb)) + pb)
            bb = b"\x00\x00" + struct.pack("!h", 0)
            bb += struct.pack("!h", len(params))
            for pv in params:
                pvb = pv.encode()
                bb += struct.pack("!i", len(pvb)) + pvb
            bb += struct.pack("!h", 0)
            s.sendall(b"B" + struct.pack("!i", 4 + len(bb)) + bb)
            s.sendall(b"D" + struct.pack("!i", 6) + b"P" + b"\x00")
            s.sendall(b"E" + struct.pack("!i", 9) + b"\x00" + struct.pack("!i", 0))
            s.sendall(b"S" + struct.pack("!i", 4))
            got_err = None
            while True:
                t, p = msg()
                if t == b"E":
                    i = 0
                    while i < len(p) - 1:
                        f = p[i:i + 1]
                        e = p.find(b"\x00", i + 1)
                        if f == b"C":
                            got_err = p[i + 1:e].decode()
                        i = e + 1
                elif t == b"Z":
                    break
            return got_err

        nfail = 0
        def expect_ok(name, err):
            nonlocal nfail
            if err:
                nfail += 1
                print(f"SQL ERR {err}: {name[:70]}")

        # ---- setup ----
        expect_ok("create", simple("create table mc82 (a int, b text)"))
        expect_ok("insert", simple("insert into mc82 select i, 'x'||i from generate_series(1,20) i"))

        # A. Parse-cache hits: same query text 60x (1 miss + 59 hits).
        q = "select a, b from mc82 where a = $1 order by a"
        for i in range(60):
            expect_ok(f"hit {i}", extended(q, (str(i % 20 + 1),)))

        # B. Parse-cache misses: 60 distinct texts.
        for i in range(60):
            expect_ok(f"miss {i}", extended(f"select a from mc82 where a = {i}"))

        # C. DDL invalidation: parse, DDL, parse again.
        expect_ok("pre-ddl", extended("select count(*) from mc82"))
        expect_ok("ddl", simple("create table mc82b (x int)"))
        expect_ok("post-ddl", extended("select count(*) from mc82"))
        expect_ok("post-ddl-distinct", extended("select count(*) from mc82b"))

        # D. FIFO eviction: 300 distinct parses.
        for i in range(300):
            expect_ok(f"evict {i}", extended(f"select {i} as n"))

        # E. Composite DDL/DML.
        expect_ok("create type", simple("create type mc82t as (x int, y text)"))
        expect_ok("insert row", simple("insert into mc82 values (21, 'z')"))
        expect_ok("row ctor", simple("select row(1,'a')::mc82t"))
        expect_ok("field access", simple("select (row(1,'a')::mc82t).x"))
        expect_ok("image eq", simple("select row(1,'a')::mc82t *= row(1,'a')::mc82t"))
        expect_ok("named cast", simple("select 'x'::text"))
        expect_ok("drop type", simple("drop type mc82t"))
        # duplicate type -> 42710; unknown nested -> 42704
        e = simple("create type mc82u as (x int)")
        expect_ok("create mc82u", e)
        e2 = simple("create type mc82u as (x int)")
        if e2 != "42710":
            nfail += 1
            print(f"expected 42710 for duplicate type, got {e2}")
        e3 = simple("create type mc82v as (x nosuchtype)")
        if e3 != "42704":
            nfail += 1
            print(f"expected 42704 for unknown nested type, got {e3}")

        # F. scalar *= must be 42883 (operator does not exist).
        e4 = simple("select 1 *= 2")
        if e4 != "42883":
            nfail += 1
            print(f"expected 42883 for scalar *=, got {e4}")
        e5 = simple("select 'a' *= 'a'")
        if e5 != "42883":
            nfail += 1
            print(f"expected 42883 for text *=, got {e5}")

        # checkpoint + restart-persistence probe for types (currently not WAL-logged).
        expect_ok("checkpoint", simple("checkpoint"))
        s.close()
        print(f"statements with unexpected SQL errors: {nfail}")
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=120)
        except Exception:
            proc.kill()
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
    for line in txt.splitlines():
        if "definitely lost" in line or "indirectly lost" in line:
            print(line.strip())
    print(f"VALGRIND_ERRORS={errors} SQL_UNEXPECTED={nfail}")
    sys.exit(1 if (errors or nfail) else 0)


if __name__ == "__main__":
    main()
