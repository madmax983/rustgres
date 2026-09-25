#!/usr/bin/env python3
"""memcheck_v094.py — run the v0.94 PG19-parity paths under valgrind memcheck.

Exercises:
  A. NaN = NaN and -0.0 = 0.0 hash joins (float8 + numeric), both sides.
  B. Cross-scale numeric hash keys (5 = 5.0 = 5.00) and cross-width int
     keys (smallint = int = bigint).
  C. GROUP BY / DISTINCT canonicalization (-0.0/0.0, NaN, scales).
  D. ORDER BY floats with NaN (pg_float_ord), scalar NaN comparisons,
     IS DISTINCT FROM on NaN.
Clean shutdown; memcheck must report 0 errors.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5594
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

QUERIES = [
    "create table m94f(a float8, v text);",
    "insert into m94f values (1.5,'a'),(-0.0,'b'),(0.0,'c'),('NaN','d'),(2.5,'e');",
    "create table m94f2(a float8, w text);",
    "insert into m94f2 values (0.0,'x'),(-0.0,'y'),('NaN','z'),(1.5,'w');",
    "select a.v, b.w from m94f a join m94f2 b on a.a = b.a order by 1,2;",
    "select a.v, b.w from m94f a join m94f2 b on a.a = b.a and a.v <> b.w order by 1,2;",
    "select a.v, b.w from m94f a left join m94f2 b on a.a = b.a order by 1,2;",
    "create table m94n(a numeric, v text);",
    "insert into m94n values (5,'a'),(5.0,'b'),(5.00,'c'),('NaN','d'),(7,'e'),('Infinity','i'),('-Infinity','ni');",
    "create table m94n2(a numeric, w text);",
    "insert into m94n2 values (5.000,'x'),('NaN','y'),(7.0,'z');",
    "select a.v, b.w from m94n a join m94n2 b on a.a = b.a order by 1,2;",
    "create table m94i(a int, v text);",
    "insert into m94i values (5,'i5'),(6,'i6');",
    "create table m94bi(a bigint, w text);",
    "insert into m94bi values (5,'b5'),(7,'b7');",
    "create table m94s(a smallint, v text);",
    "insert into m94s values (5,'s5');",
    "select i.v, b.w from m94i i join m94bi b on i.a = b.a;",
    "select s.v, b.w from m94s s join m94bi b on s.a = b.a;",
    "select i.v, n.w from m94i i join m94n2 n on i.a = n.a;",
    # GROUP BY / DISTINCT canonicalization
    "select a, count(*) from m94f group by a order by 1;",
    "select count(*) from (select distinct a from m94f) t;",
    "select count(*) from (select distinct a from m94n) t;",
    "select a, count(*) from m94n group by a order by 1;",
    # ORDER BY floats with NaN / signed zero
    "select v from m94f order by a;",
    "select v from m94f order by a desc;",
    "select v from m94n order by a;",
    # scalar NaN comparisons
    "select 'nan'::float8 = 'nan'::float8, 'nan'::float8 <> 'nan'::float8;",
    "select 'nan'::numeric = 'nan'::numeric, 'nan'::numeric <> 'nan'::numeric;",
    "select -0.0::float8 = 0.0::float8, -0.0::float8 < 0.0::float8;",
    "select 'nan'::float8 is distinct from 'nan'::float8;",
    "select 'nan'::numeric is not distinct from 'nan'::numeric;",
    # aggregates over the canonicalized groups
    "select count(*), min(a), max(a) from m94f;",
    "select count(distinct a) from m94f;",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc94_")
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
            print("server did not start under valgrind")
            proc.kill()
            sys.exit(2)
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

        def read_msg():
            t = rd(1)
            ln = struct.unpack("!I", rd(4))[0]
            return t, rd(ln - 4)

        # consume startup messages until ReadyForQuery
        while True:
            t, _ = read_msg()
            if t == b"Z":
                break

        def q(sql):
            s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1)
                      + sql.encode() + b"\x00")
            while True:
                t, p = read_msg()
                if t == b"Z":
                    return
                if t == b"E":
                    print(f"UNEXPECTED ERROR for {sql}: {p!r}")
                    sys.exit(1)

        for sql in QUERIES:
            q(sql)
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    txt = open(log).read()
    # summarize
    import re
    m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
    errs = m.group(1) if m else "?"
    print(f"memcheck: ERROR SUMMARY: {errs} errors")
    if errs != "0":
        for line in txt.splitlines():
            if "ERROR SUMMARY" in line or re.match(r"==\d+== .* (lost|error)", line):
                print(" ", line.strip()[:120])
        sys.exit(1)
    print("memcheck v0.94: clean")


if __name__ == "__main__":
    main()
