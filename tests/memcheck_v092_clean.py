#!/usr/bin/env python3
"""memcheck_v092_clean.py — v0.92 aggregate paths under valgrind memcheck
with a CLEAN server exit (exit code 0).

Uses the v0.92 test-only `RUSTGRES_MAX_CONN` mode: the server serves the
readiness probe (1 connection) plus the workload connection, then joins
its handlers and exits 0. Valgrind therefore reports against a normal
process exit — no SIGTERM, no -15.

Workload: same QUERIES as memcheck_v092.py (array_agg NULL semantics,
anyarray error paths, ORDER BY in aggregates, windowed rejections,
Knuth division guard).
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5594
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
        err = None
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
            elif t == b"E":
                # error: capture SQLSTATE-ish, then wait for Ready
                err = p.decode(errors="replace")
            elif t == b"Z":
                break
        return rows, err

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
    # B2. static overload discrimination (v0.92 fix): all-NULL array-typed
    "select array_agg(null::int[]) from (values (1),(2)) v(x);",
    "select array_agg(null::int[]) over () from (values (1),(2)) v(x);",
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
    data_dir = tempfile.mkdtemp(prefix="rgmc92c_")
    log = os.path.join(data_dir, "vg.log")
    # MAX_CONN=2: the readiness probe opens+closes one connection, the
    # workload uses the second; then the server joins handlers and exits 0.
    env = dict(os.environ, PGDATESTYLE="Postgres, MDY", RUSTGRES_MAX_CONN="2")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         f"--suppressions={os.path.join(os.path.dirname(os.path.abspath(__file__)), 'valgrind.supp')}",
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
        # The server should exit 0 on its own after the 2nd connection.
        rc = proc.wait(timeout=60)
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
    rc = proc.returncode
    with open(log) as f:
        log_text = f.read()
    print(f"valgrind exit={rc} log={log}")
    summaries = [l for l in log_text.splitlines() if "ERROR SUMMARY" in l]
    for s in summaries:
        print(s)
    leaks = [l for l in log_text.splitlines()
             if "definitely lost" in l or "indirectly lost" in l or "possibly lost" in l]
    for l in leaks:
        print(l.strip())
    ok = (rc == 0 and all("0 errors" in s for s in summaries)
          and all(x.split(":")[1].strip().startswith("0 bytes") for x in leaks))
    print("CLEAN-EXIT MEMCHECK:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
