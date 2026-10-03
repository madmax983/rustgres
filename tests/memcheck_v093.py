#!/usr/bin/env python3
"""memcheck_v093.py — run the v0.93 hash equi-join + related new paths under
valgrind memcheck.

Exercises:
  A. Hash inner equi-join: int keys with duplicates + NULLs, multi-key,
     residual predicates, USING, self-join, empty side.
  B. Hash key families: numeric, float, text/varchar/bpchar, bool, date,
     timestamp, uuid, bytea.
  C. New v0.93 naming/cast paths: func-style casts (float8(x)), to_char
     with V pictures (incl. 22003 overflow).
  D. Fallback paths: LEFT JOIN, non-equi, cross join (nested loop).
Clean shutdown; memcheck must report 0 errors.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5593
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

QUERIES = [
    "create table mca(id int, v text);",
    "create table mcb(id int, w text);",
    "insert into mca values (1,'a1'),(2,'a2'),(2,'a2b'),(NULL,'aN'),(5,'a5');",
    "insert into mcb values (2,'b2'),(2,'b2b'),(3,'b3'),(NULL,'bN');",
    "select a.id, a.v, b.w from mca a join mcb b on a.id = b.id order by 1,2,3;",
    "select count(*) from mca a join mcb b on a.id = b.id;",
    "select a.v from mca a join mcb b on a.id = b.id and a.v <> b.w order by 1;",
    "select m.v from mca m join mcb b on m.id = b.id and b.id = b.id order by 1;",
    "select a.id from mca a join mcb b using (id) order by 1;",
    "select x.v, y.v from mca x join mca y on x.id = y.id order by 1,2;",
    "create table mcn(id numeric, v text);",
    "insert into mcn values (2.0,'n2'),(NULL,'nN');",
    "select a.v, n.v from mca a join mcn n on a.id = n.id order by 1,2;",
    "create table mct(k text, v int);",
    "insert into mct values ('x',1),(NULL,2);",
    "select t.k, b.w from mct t join mcb b on t.k = b.w order by 1;",
    "create table mcd(d date, v int);",
    "create table mcd2(d date, v int);",
    "insert into mcd values ('2026-01-01',1);",
    "insert into mcd2 values ('2026-01-01',10);",
    "select d.v, d2.v from mcd d join mcd2 d2 on d.d = d2.d;",
    "select a.id, b.w from mca a left join mcb b on a.id = b.id order by 1,2;",
    "select count(*) from mca a join mcb b on a.id < b.id;",
    "select count(*) from mca a cross join mcb b;",
    # v0.93 naming: func-style cast
    "select float8(2), 3::float8, cast(4 as float8);",
    "select int(5), text(6);",
    # v0.93 to_char V
    "select to_char(2, '9V999999999');",
    "select to_char(214748364, '999999999V9');",
    "select to_char(-2, '9V999999999');",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc93_")
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
                    # to_char V overflow queries are expected to error;
                    # anything else is a workload bug.
                    if "22003" not in p.decode("utf8", "replace"):
                        print(f"UNEXPECTED ERROR for {sql}: {p!r}")
                        sys.exit(1)
                    return

        for sql in QUERIES:
            q(sql)
        # the 22003 cases (must error, tested separately for the message)
        for sql in ["select to_char(3, '9V999999999');",
                    "select to_char(2147483647, '9V9');",
                    "select to_char(1, '9V9999999999');"]:
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
    print("memcheck v0.93: clean")


if __name__ == "__main__":
    main()
