#!/usr/bin/env python3
"""memcheck_v079.py — run the v0.79 array paths under valgrind memcheck.

Exercises: array DDL (int[]/text[]/int[][]), INSERT of '{...}' literals,
SELECT array_out rendering, ARRAY[...] constructor, array(SELECT ...),
subscripts (1-based, OOB -> NULL, partial -> NULL, >6 dims -> 54000,
non-integer index -> 42804, subscript of non-array -> 42804), slices
(bound reset, empty slice), array_length/cardinality/array_dims/
array_ndims/array_lower/array_upper, unnest as table function / scalar
SRF / nested scalar, casts (text->int[] incl. 22P02 malformed,
int[]->text, int[]->bigint[]), array = / <>, || concatenation
(array||array, array||elem, elem||array), plus an extended-protocol
Describe of a subscript query.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5549
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # A. Array DDL + literals + rendering
    "create table m79a(id int, v int[], t text[], m int[][])",
    "insert into m79a values (1, '{1,2,3}', '{\"a,b\",\"c\"}', '{{1,2},{3,4}}')",
    "insert into m79a values (2, '{1,NULL,3}', '{}', null)",
    "select id, v, t, m from m79a order by id",
    "select pg_typeof(v), pg_typeof(t), pg_typeof(m) from m79a limit 1",
    # B. ARRAY[...] constructor
    "select array[1,2,3], array['a','b'], array[]::int[]",
    "select pg_typeof(array[1,2,3]), pg_typeof(array['a'])",
    # C. array(SELECT ...)
    "select array(select generate_series(1,3))",
    "select pg_typeof(array(select 1))",
    # D. Subscripts
    "select (array[10,20,30])[1], (array[10,20,30])[3]",
    "select (array[10,20,30])[0], (array[10,20,30])[4], (array[10,20,30])[-1]",
    "select ('{{1,2},{3,4}}'::int[])[2][1]",
    "select ('{{1,2},{3,4}}'::int[])[2]",
    "select (null::int[])[1], (array[1])[null]",
    "select ('{1}'::int[])[1][1][1][1][1][1][1]",
    "select 5[1]",
    "select (array[1])['x']",
    "select (array[1])[1.5]",
    "select v[1], v[2:3] from m79a order by id",
    # E. Slices
    "select ('{1,2,3,4}'::int[])[2:3]",
    "select array_dims(('{1,2,3,4}'::int[])[2:3])",
    "select ('{1,2}'::int[])[3:1]",
    "select ('{{1,2,3},{4,5,6}}'::int[])[1:2][2:3]",
    "select ('{1,2,3}'::int[])[:2], ('{1,2,3}'::int[])[2:]",
    "select (array[1,2])['a':2]",
    # F. Array functions
    "select array_length(array[1,2,3],1), array_length('{{1,2},{3,4}}'::int[],2)",
    "select cardinality(array[1,2,3]), cardinality('{}'::int[])",
    "select array_dims('{{1,2},{3,4}}'::int[]), array_ndims('{{1,2},{3,4}}'::int[])",
    "select array_lower('[0:2]={1,2,3}'::int[],1), array_upper('[0:2]={1,2,3}'::int[],1)",
    "select array_length(null::int[],1), cardinality(null::int[])",
    # G. unnest
    "select * from unnest(array[1,2,3])",
    "select unnest(array['a','b'])",
    "select * from unnest(null::int[])",
    "select pg_typeof(unnest(array['a']))",
    # H. Casts
    "select '{1,2,3}'::int[], '{1,NULL,3}'::int[]",
    "select (array[1,2])::text, (array[1,2])::bigint[]",
    "select '{a,b}'::int[]",
    "select '{{1,2},{3}}'::int[]",
    # I. = / <>
    "select array[1,2] = array[1,2], array[1,2] = array[1,3]",
    "select array[1,null] = array[1,null], array[1,2] <> array[1,3]",
    # J. ||
    "select array[1,2] || array[3,4], array[1,2] || 3, 0 || array[1,2]",
    "select array[]::int[] || array[1]",
    # WAL/checkpoint roundtrip of array data
    "checkpoint",
    "select count(*) from m79a",
]

# Extended-protocol Describe of a subscript + unnest query.
EXT_DESCRIBE_SQL = ("select v[1], unnest(v) from m79a")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc79_")
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
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
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

        # Error paths are exercised deliberately; only unexpected codes count.
        expect_err = {"54000", "42804", "22P02"}
        nfail = 0
        for q in STMTS:
            err = simple(q)
            if err and err not in expect_err:
                nfail += 1
                print(f"SQL ERR {err}: {q[:60]}")

        # Extended protocol: Parse + Describe + Sync.
        sq = EXT_DESCRIBE_SQL.encode()
        parse = (b"P" + struct.pack("!i", 0)
                 + b"\x00" + sq + b"\x00" + struct.pack("!h", 0))
        parse = b"P" + struct.pack("!i", len(parse) - 1) + parse[5:]
        desc = b"D" + struct.pack("!i", 6) + b"S" + b"\x00"
        sync = b"S" + struct.pack("!i", 4)
        s.sendall(parse + desc + sync)
        saw_t = False
        while True:
            t, p = msg()
            if t == b"T":
                saw_t = True
            elif t == b"E":
                print("extended Describe returned an error (unexpected)")
                nfail += 1
            elif t == b"Z":
                break
        print(f"extended Describe returned RowDescription: {saw_t}")
        if not saw_t:
            nfail += 1
        s.close()
        print(f"statements with unexpected SQL errors: {nfail}")
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=60)
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
    if errors:
        print("--- first 30 error lines ---")
        n = 0
        for line in txt.splitlines():
            if "valgrind" in line.lower() or "Invalid" in line or "uninitialised" in line:
                print(line.rstrip())
                n += 1
                if n >= 30:
                    break
    sys.exit(1 if errors or nfail else 0)


if __name__ == "__main__":
    main()
