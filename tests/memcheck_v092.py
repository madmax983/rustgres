#!/usr/bin/env python3
"""memcheck_v092.py — run the v0.92 array_agg + ORDER BY-in-aggregate paths
under valgrind memcheck.

Exercises every new v0.92 path:
  A. array_agg scalar NULLs kept ({1,NULL,3}, {NULL,NULL}, empty -> NULL),
     DISTINCT with NULLs, grouped + windowed NULL retention.
  B. array_agg(anyarray): NULL -> 22004, empty first -> 2202E,
     empty later / dim mismatch -> 2202E, compatible stacking.
  C. ORDER BY in grouped aggregates: array_agg ASC/DESC/NULLS FIRST/LAST,
     string_agg with per-row delimiters, sum, grouped ORDER BY,
     DISTINCT + ORDER BY (ok + 42P10 rejection).
  D. Windowed rejections: ORDER BY in windowed agg -> 0A000,
     DISTINCT windowed agg -> 0A000.
  E. Knuth div_rem hot path via numeric division (regression-guard the
     v0.90 BigUint rewrite under memcheck).

The server is started with PGDATESTYLE=Postgres, MDY.

Strictness note: the server has no clean-shutdown path (pure-std, no
signal handling), so it is SIGTERMed; memcheck still reports every
invalid access (ERROR SUMMARY must be 0). The full leak-kind proof
runs separately against the unit-test binary, which exits cleanly.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5593
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def cstr(s):
    return s.encode() + b"\x00"


def msg(t, payload):
    return t + struct.pack("!I", 4 + len(payload)) + payload


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("closed")
        d += c
    return d


def read_msg(s):
    t = read_exact(s, 1)
    ln = struct.unpack("!I", read_exact(s, 4))[0]
    return t, read_exact(s, ln - 4)


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=15)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!I", 8 + len(params)) + b"\x00\x03\x00\x00" + params)
        while True:
            t, p = read_msg(self.s)
            if t == b"Z":
                break

    def q(self, sql):
        self.s.sendall(msg(b"Q", cstr(sql)))
        rows = []
        while True:
            t, p = read_msg(self.s)
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos+4])
                    pos += 4
                    if ln == -1:
                        r.append(None)
                    else:
                        r.append(p[pos:pos+ln].decode(errors="replace"))
                        pos += ln
                rows.append(r)
            elif t in (b"E", b"Z"):
                if t == b"Z":
                    break
        return rows

    def close(self):
        self.s.close()


QUERIES = [
    # A. array_agg NULL semantics (v0.92 correction)
    "select array_agg(i) from generate_series(1,100) g(i);",
    "select array_agg(i) from (values (1),(null),(3)) v(i);",
    "select array_agg(i) from (values (null),(null)) v(i);",
    "select array_agg(i) from (select 1 as i where false) t;",
    "select array_agg(distinct i) from (values (1),(null),(1),(null)) v(i);",
    "select g, array_agg(i) from (values (1,1),(1,null),(2,3)) v(g,i) group by g;",
    "select array_agg(i) over () from (values (1),(null),(3)) v(i);",
    # B. array_agg(anyarray) error paths
    "select array_agg(a) from (values (array[1,2]),(null)) v(a);",
    "select array_agg(a) from (values (null),(array[1,2])) v(a);",
    "select array_agg(a) from (values ('{}'::int[]),(array[1,2])) v(a);",
    "select array_agg(a) from (values (array[1,2]),('{}'::int[])) v(a);",
    "select array_agg(a) from (values (array[1,2]),(array[1,2,3])) v(a);",
    "select array_agg(a) from (values (array[1,2]),(array[1,2])) v(a);",
    "select array_agg(array[i, i+1]) from generate_series(1,20) g(i);",
    # C. ORDER BY in grouped aggregates
    "select array_agg(x order by y) from (values (1,3),(2,1),(3,2)) v(x,y);",
    "select array_agg(x order by y desc) from (values (1,3),(2,1),(3,2)) v(x,y);",
    "select array_agg(x order by y nulls first) from (values (1,1),(2,null),(3,2)) v(x,y);",
    "select string_agg(x, ',' order by y) from (values ('a',2),('b',1)) v(x,y);",
    "select string_agg(x, d order by y) from (values ('a','-',2),('b','+',1)) v(x,d,y);",
    "select sum(x order by y) from (values (1,2),(2,1)) v(x,y);",
    "select g, array_agg(x order by y) from (values (1,'b',2),(1,'a',1)) v(g,x,y) group by g;",
    "select array_agg(distinct x order by x) from (values (2),(1),(2)) v(x);",
    "select string_agg(distinct x, d order by x, d) from (values ('a','-'),('a','+')) v(x,d);",
    "select array_agg(distinct x order by y) from (values (1,2),(2,1)) v(x,y);",  # 42P10
    # D. windowed rejections
    "select array_agg(x order by y) over () from (values (1,2)) v(x,y);",  # 0A000
    "select sum(distinct x) over () from (values (1),(1)) v(x);",  # 0A000
    "select sum(x) over () from (values (1),(2)) v(x);",
    # E. Knuth div_rem regression guard (v0.90 rewrite)
    "select 1/3::numeric;",
    "select ln(2::numeric);",
    "select sqrt(2::numeric);",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc92_")
    log = os.path.join(data_dir, "vg.log")
    env = dict(os.environ, PGDATESTYLE="Postgres, MDY")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("server did not start")
            return 1
        c = Conn()
        for sql in QUERIES:
            try:
                c.q(sql)
            except Exception as e:
                print(f"query failed (continuing): {sql[:60]!r}: {e}")
        c.close()
        time.sleep(1)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    rc = proc.returncode
    with open(log) as f:
        log_text = f.read()
    print(f"valgrind exit={rc} log={log}")
    # ERROR SUMMARY lines: every one must show 0 errors.
    summaries = [l for l in log_text.splitlines() if "ERROR SUMMARY" in l]
    for s in summaries:
        print(s)
    bad = [s for s in summaries if "ERROR SUMMARY: 0" not in s]
    if rc == 99 or bad:
        print("MEMCHECK FAILURES DETECTED")
        return 1
    # Invalid reads/writes would also appear as named error blocks.
    for kind in ("Invalid read", "Invalid write", "Conditional jump"):
        hits = [l for l in log_text.splitlines() if kind in l]
        if hits:
            print(f"{kind}: {len(hits)} occurrences (FAIL)")
            for h in hits[:5]:
                print("  " + h)
            return 1
    print("memcheck clean: ERROR SUMMARY 0, no invalid accesses")
    return 0


if __name__ == "__main__":
    sys.exit(main())
