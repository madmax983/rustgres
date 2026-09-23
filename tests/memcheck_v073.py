#!/usr/bin/env python3
"""memcheck_v073.py — run the v0.73 new-code paths under valgrind memcheck.

Exercises: whole-row Vars (bare relation refs, qual.* in expression
position, (r.*)::text casts, row_to_json(r.*), count(r.*) and IS NULL
null semantics, INSERT...RETURNING tbl incl. partitioned parent-order),
correlated whole-row subqueries (the describe outer-schema threading),
USING-alias whole-row shape, and the preserved 42703 error paths.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5547
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # whole-row basics
    "create table m_wr(a int, b text)",
    "insert into m_wr values (1,'foo'),(2,null)",
    "select m_wr from m_wr",
    "select (m_wr.*)::text from m_wr",
    "select m_wr.* from m_wr",
    "select row_to_json(m_wr.*) from m_wr",
    "select count(m_wr.*) from m_wr",
    "select (m_wr.*) is null from m_wr",
    "insert into m_wr values (3,'bar') returning m_wr",
    # views + correlated whole-row subqueries
    "create table m_ta(id integer)",
    "insert into m_ta values (42)",
    "create view m_va as select * from m_ta",
    "select m_va from m_va",
    "select (select m_va) from m_va",
    "select (select (select m_va)) from m_va",
    "select (select (a.*)::text) from m_va a",
    "select q from (select max(id) from m_ta group by id) q",
    # USING-alias shape
    "create table m_j1(i integer)",
    "create table m_j2(i integer)",
    "insert into m_j1 values (1)",
    "insert into m_j2 values (1)",
    "select x.* from m_j1 join m_j2 using (i) as x",
    "select row_to_json(x.*) from m_j1 join m_j2 using (i) as x",
    # outer-join null semantics
    "create table m_o1(a int)",
    "create table m_o2(a int)",
    "insert into m_o1 values (1),(2)",
    "insert into m_o2 values (2),(3)",
    "select count(m_o2.*) from m_o1 left join m_o2 on m_o1.a = m_o2.a",
    "select (m_o2.*) is null from m_o1 left join m_o2 on m_o1.a = m_o2.a",
    # partitioned RETURNING parent order
    "create table m_prt (a int) partition by list (a)",
    "create table m_prt1 partition of m_prt for values in (1)",
    "insert into m_prt values (1) returning m_prt",
    "alter table m_prt add b text",
    "create table m_prt2 (b text, c int, a int)",
    "alter table m_prt2 drop c",
    "alter table m_prt attach partition m_prt2 for values in (2)",
    "insert into m_prt values (2, 'foo') returning m_prt",
    # preserved error paths
    "select nosuchcol from m_wr",
    "select nosuch.* from m_wr",
    "checkpoint",
]

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc_")
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
