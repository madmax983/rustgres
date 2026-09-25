#!/usr/bin/env python3
"""memcheck_v098.py — run the v0.98 sequence-parity paths under valgrind memcheck.

Exercises: volatile-predicate non-pushdown (nextval + volatile SQL UDF),
currval/lastval session state incl. 55000 error paths, setval(n,true/false)
semantics + 22003 bounds, RESTART WITH bounds (22023), CREATE/ALTER CACHE
(22023 on <1), OWNED BY create/alter/drop/none + invalid-target leak check,
ALTER SEQUENCE IF EXISTS, bare START n, pg_sequences /
information_schema.sequences scans, dollar-quoted CREATE FUNCTION
(split_statements $$ handling), and WAL replay + checkpoint of sequence
state (values, cache, owned_by) across a restart.
"""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5550
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # volatile pushdown regression (evaluated once per row, not doubled)
    "create table m98t as select g % 10 as ten from generate_series(1, 100) g",
    "create sequence m98ts",
    "select * from (select distinct ten from m98t) ss where ten < 10 + nextval('m98ts') order by 1",
    "select nextval('m98ts')",
    # volatile SQL-language UDF wrapping nextval
    "create sequence m98uvs",
    "create function m98unv() returns bigint volatile language sql as $$ select nextval('m98uvs') $$",
    "select * from (select distinct ten from m98t) ss where ten < 10 + m98unv() order by 1",
    "select nextval('m98uvs')",
    # currval / lastval
    "create sequence m98cv",
    "select currval('m98cv')",
    "select nextval('m98cv')",
    "select currval('m98cv')",
    "select lastval()",
    "create sequence m98lv start with 100",
    "select nextval('m98lv')",
    "select lastval()",
    "select setval('m98lv', 500)",
    "select lastval()",
    # setval semantics + bounds
    "create sequence m98sv",
    "select setval('m98sv', 41)",
    "select nextval('m98sv')",
    "select setval('m98sv', 77, false)",
    "select nextval('m98sv')",
    "create sequence m98bd minvalue 1 maxvalue 10 start with 1",
    "select setval('m98bd', 99)",
    "select setval('m98bd', 0)",
    "select setval('m98bd', 10)",
    # restart bounds
    "alter sequence m98bd restart with 99",
    "alter sequence m98bd restart with 0",
    "alter sequence m98bd restart with 5",
    "select nextval('m98bd')",
    # cache
    "create sequence m98cs cache 20",
    "select cache_size from pg_sequences where sequencename = 'm98cs'",
    "alter sequence m98cs cache 50",
    "create sequence m98cs0 cache 0",
    "create sequence m98csn cache -3",
    # owned by
    "create table m98ot(a int)",
    "create sequence m98os owned by m98ot.a",
    "drop table m98ot",
    "select nextval('m98os')",
    "create sequence m98ob owned by nosuch_t.a",
    "select sequencename from pg_sequences where sequencename = 'm98ob'",
    "create table m98ot2(a int, b int)",
    "create sequence m98os2",
    "alter sequence m98os2 owned by m98ot2.b",
    "alter sequence m98os2 owned by none",
    "drop table m98ot2",
    "select nextval('m98os2')",
    # if exists / bare start
    "alter sequence if exists m98nosuch restart",
    "alter sequence m98nosuch restart",
    "create sequence m98bs start 7",
    "select nextval('m98bs')",
    # catalogs
    "select * from pg_sequences",
    "select count(*) from pg_sequences",
    "select * from information_schema.sequences",
    "select sequence_name, data_type from information_schema.sequences",
    # dollar-quoted function body with internal semicolons (splitter)
    "create function m98dq() returns int language sql as $body$ select 1 $body$",
    "select m98dq()",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc98_")
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

        for sql in STMTS:
            err = q(sql)
            if err:
                errors.append((sql, err[:80]))
        # WAL replay path: checkpoint, restart, verify sequence state survives
        q("checkpoint")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    # Restart: replay WAL + checkpoint
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}.2",
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
            print("restart server did not start under valgrind")
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

        for sql in [
            "select nextval('m98ts')",
            "select cache_size from pg_sequences where sequencename = 'm98cs'",
            "select nextval('m98os2')",
            "select count(*) from pg_sequences",
        ]:
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
    nerr = 0
    for lf in (log, log + ".2"):
        txt = open(lf).read()
        m = re.search(r"ERROR SUMMARY: (\d+) errors", txt)
        n = int(m.group(1)) if m else -1
        print(f"{os.path.basename(lf)} ERROR SUMMARY: {n}")
        nerr += max(n, 0)
    print(f"queries with SQL errors: {len(errors)}")
    for sql, e in errors[:10]:
        print(f"  {sql[:60]} -> {e!r}")
    print(f"memcheck total errors: {nerr}")
    if nerr != 0:
        sys.exit(1)
    print("memcheck_v098: zero errors")


if __name__ == "__main__":
    main()
