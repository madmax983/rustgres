#!/usr/bin/env python3
"""memcheck_v095.py — run the v0.95 PG19-parity paths under valgrind memcheck.

Exercises:
  A. parse_ident (quoted/unquoted folding, multi-part, invalid syntax,
     strict and non-strict) via positional and named-arg calls.
  B. Named-argument calls: parse_ident(qualname => ..., strict => ...),
     UDF with named args (out-of-order), positional-before-named ordering.
  C. HAVING whole-expression grouping (SELECT lower(c) GROUP BY lower(c)).
  D. Degenerate grouping: HAVING without GROUP BY skips WHERE evaluation
     (SELECT 1 FROM t WHERE 1/a=1 HAVING 1<2 -> 1 row, no 22012).
  E. Virtual pg_class self-row (OID 1259).
  F. Derived-table enclosing scope (correlated derived table).
Clean shutdown; memcheck must report 0 errors.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5595
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

QUERIES = [
    # A. parse_ident
    "select parse_ident('Foo.Bar');",
    "select parse_ident('\"Foo\".Bar');",
    "select parse_ident('a.b.c.d');",
    "select parse_ident('simple');",
    "select parse_ident('a.b.c.d.e');",          # >3 parts (PG allows 4)
    "select parse_ident('\"a b\".c');",
    "select parse_ident('a..b', false);",        # non-strict: valid prefix
    "select parse_ident('\"unterminated', false);",
    "select parse_ident('a b', false);",
    # B. named args
    "select parse_ident(qualname => 'A.b');",
    "select parse_ident(qualname => 'A.b', strict => false);",
    "select parse_ident(strict => true, qualname => '\"X\".y');",
    "create function add95(alpha int, beta int) returns int language sql as $$select alpha + beta$$;",
    "select add95(beta => 20, alpha => 1);",
    "select add95(1, beta => 2);",
    # C. HAVING whole-expression grouping
    "create table h95(c text);",
    "insert into h95 values ('A'),('b'),('C'),('a');",
    "select lower(c), count(*) from h95 group by lower(c) order by 1;",
    "select lower(c) from h95 group by lower(c) having count(*) > 1;",
    # D. degenerate grouping
    "create table d95(a int);",
    "insert into d95 values (1),(2),(0);",
    "select 1 from d95 where 1/a = 1 having 1 < 2;",
    "select count(*) from d95 having 2 > 1;",
    # E. virtual pg_class self-row
    "select oid, relname from pg_class where oid = 1259;",
    "select count(*) from pg_class where relname = 'pg_class';",
    # F. derived-table enclosing scope
    "create table o95(x int);",
    "insert into o95 values (1),(2),(3);",
    "select a.x, (select max(t.x) from (select x from o95 where x > a.x) t) from o95 a order by 1;",
    "select count(*) from (select a.x from o95 a where a.x in (select x from o95)) t;",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc95_")
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    errors = []
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

        while True:
            t, _ = read_msg()
            if t == b"Z":
                break

        def q(sql):
            s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1)
                      + sql.encode() + b"\x00")
            err = None
            while True:
                t, p = read_msg()
                if t == b"Z":
                    return err
                if t == b"E":
                    err = p

        for sql in QUERIES:
            err = q(sql)
            if err:
                errors.append((sql, err[:80]))
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    txt = open(log).read()
    import re
    m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
    nerr = int(m.group(1)) if m else -1
    print(f"queries with SQL errors: {len(errors)}")
    for sql, e in errors[:10]:
        print(f"  {sql[:60]} -> {e!r}")
    print(f"memcheck ERROR SUMMARY: {nerr}")
    if nerr != 0:
        for line in txt.splitlines():
            if "definitely lost" in line or "indirectly lost" in line:
                print(" ", line.strip())
        sys.exit(1)
    print("MEMCHECK CLEAN")


if __name__ == "__main__":
    main()
