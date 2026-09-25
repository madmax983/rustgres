#!/usr/bin/env python3
"""memcheck_v088.py — run every new v0.88 path under valgrind memcheck.

Exercises:
  A. CREATE INDEX key options: DESC / NULLS FIRST / NULLS LAST
     (single + multi-key), then ORDER BY both directions.
  B. Expression indexes: catalog-only create, DML (insert/update/delete)
     with the catalog-only index present, then drop.
  C. Partial indexes (WHERE predicate): catalog-only create, DML, drop.
  D. DROP INDEX with comma-separated names (all present; one missing).
  E. Virtual pg_attribute: full scan + the conformance RIGHT JOIN
     introspection query.
  F. b_star join-suite fixture query shape (RIGHT JOIN + impossible
     predicate).
  G. CREATE INDEX inside a transaction + ROLLBACK.
  H. CHECKPOINT with the new index metadata present.

Valgrind must report zero errors; every SQL expectation must hold.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5590
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc88_")
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

        def errcode(p):
            code = None
            i = 0
            while i < len(p) - 1:
                f = p[i:i + 1]
                e = p.find(b"\x00", i + 1)
                if e < 0:
                    break
                if f == b"C":
                    code = p[i + 1:e].decode()
                i = e + 1
            return code

        def simple(q):
            qb = q.encode()
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            rows = []
            got_err = None
            while True:
                t, p = msg()
                if t == b"D":
                    n = struct.unpack("!h", p[:2])[0]
                    off = 2
                    r = []
                    for _ in range(n):
                        ln = struct.unpack("!i", p[off:off + 4])[0]
                        off += 4
                        if ln < 0:
                            r.append(None)
                        else:
                            r.append(p[off:off + ln].decode())
                            off += ln
                    rows.append(r)
                elif t == b"E":
                    got_err = errcode(p)
                elif t == b"Z":
                    break
            return got_err, rows

        nfail = 0
        def check(name, cond, detail=""):
            nonlocal nfail
            if not cond:
                nfail += 1
                print(f"FAIL {name} {detail}")

        # ---- setup ----
        e, _ = simple("create table mc88 (a int, b int)")
        check("create", e is None, e)
        e, _ = simple("insert into mc88 values (3, 1), (1, 2), (2, 3)")
        check("insert", e is None, e)

        # A. DESC / NULLS key options
        e, _ = simple("create index mc88_d on mc88 (a desc)")
        check("desc", e is None, e)
        e, _ = simple("create index mc88_nf on mc88 (b desc nulls first)")
        check("desc nulls first", e is None, e)
        e, _ = simple("create index mc88_nl on mc88 (a asc nulls last, b desc)")
        check("multi-key", e is None, e)
        e, r = simple("select a from mc88 order by a")
        check("order asc", e is None and r == [["1"], ["2"], ["3"]], (e, r))
        e, r = simple("select a from mc88 order by a desc")
        check("order desc", e is None and r == [["3"], ["2"], ["1"]], (e, r))

        # B. expression index: catalog-only
        e, _ = simple("create unique index mc88_fn on mc88 ((a * a))")
        check("expr create", e is None, e)
        e, _ = simple("insert into mc88 values (4, 4)")
        check("insert with expr", e is None, e)
        e, _ = simple("update mc88 set b = 9 where a = 4")
        check("update with expr", e is None, e)
        e, _ = simple("delete from mc88 where a = 4")
        check("delete with expr", e is None, e)
        e, r = simple("select count(*) from mc88")
        check("count back to 3", e is None and r == [["3"]], (e, r))

        # C. partial index: catalog-only
        e, _ = simple("create index mc88_p on mc88 (b) where b > 1")
        check("partial create", e is None, e)
        e, _ = simple("insert into mc88 values (5, 5)")
        check("insert with partial", e is None, e)

        # D. multi-name DROP INDEX
        e, _ = simple("drop index mc88_d, mc88_nf, mc88_nl")
        check("multi drop", e is None, e)
        e, _ = simple("drop index mc88_fn, mc88_p")
        check("drop catalog-only", e is None, e)
        e, _ = simple("drop index mc88_nope")
        check("missing 42P01", e == "42P01", e)

        # E. pg_attribute
        e, r = simple("select attname, attnum from pg_attribute "
                      "where attname = 'b' and attrelid = "
                      "(select oid from pg_class where relname = 'mc88')")
        check("pg_attribute", e is None and r == [["b", "2"]], (e, r))
        e, r = simple(
            "select tname, attname from ("
            " select relname::information_schema.sql_identifier as tname, *"
            " from (select * from pg_class c) ss1) ss2"
            " right join pg_attribute a on a.attrelid = ss2.oid"
            " where tname = 'mc88' and attnum = 1")
        check("pg_attribute rjoin", e is None and r == [["mc88", "a"]], (e, r))

        # F. b_star fixture shape
        e, _ = simple("create table b_star (class char, aa int4, bb text)")
        check("b_star", e is None, e)
        e, r = simple("select aa, bb, a, a from mc88 "
                      "right join b_star on aa = a "
                      "where bb < bb and bb is null")
        check("b_star join", e is None and r == [], (e, r))

        # G. CREATE INDEX in txn + rollback
        e, _ = simple("begin")
        check("begin", e is None, e)
        e, _ = simple("create index mc88_rb on mc88 (a desc) where a > 0")
        check("create in txn", e is None, e)
        e, _ = simple("rollback")
        check("rollback", e is None, e)
        e, _ = simple("drop index mc88_rb")
        check("rolled back 42P01", e == "42P01", e)

        # H. checkpoint with v0.88 metadata present
        e, _ = simple("create index mc88_c on mc88 (a desc nulls first)")
        check("recreate desc", e is None, e)
        e, _ = simple("create unique index mc88_ce on mc88 ((b + 1))")
        check("recreate expr", e is None, e)
        e, _ = simple("checkpoint")
        check("checkpoint", e is None, e)

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
        print(f"checks failed: {nfail}")
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
