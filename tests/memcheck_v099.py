#!/usr/bin/env python3
"""memcheck_v099.py — run the v0.99 sequence-type / REAL-FAIL-fix paths under
valgrind memcheck.

Exercises: CREATE/ALTER SEQUENCE AS smallint|int|bigint (type-driven
defaults, 22023 on invalid type or out-of-range bounds), ALTER ... AS reset
semantics, NO MINVALUE/NO MAXVALUE reset flags, pg_sequences.cycle rename +
information_schema.sequences.cycle_option, correlated derived tables
(outer_schemas threading), parse_ident as a table function in FROM, CREATE
FUNCTION ... COST, expression indexes, and WAL replay + checkpoint of typed
sequence state (seq_type, cache, owned_by) across a restart.
"""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5551
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # typed sequences
    "create sequence m99si as smallint",
    "select nextval('m99si')",
    "select min_value, max_value from pg_sequences where sequencename = 'm99si'",
    "create sequence m99i as int",
    "select min_value, max_value from pg_sequences where sequencename = 'm99i'",
    "create sequence m99bi as bigint",
    "select min_value, max_value from pg_sequences where sequencename = 'm99bi'",
    "create sequence m99bad as text",
    "create sequence m99oor as smallint maxvalue 100000",
    "create sequence m99oor2 as smallint minvalue -100000",
    "create sequence m99d as smallint increment by -1",
    "select nextval('m99d')",
    # alter AS reset semantics
    "create sequence m99a",
    "alter sequence m99a as smallint",
    "select min_value, max_value from pg_sequences where sequencename = 'm99a'",
    "create sequence m99b maxvalue 500",
    "alter sequence m99b as smallint",
    "select max_value from pg_sequences where sequencename = 'm99b'",
    # NO MINVALUE / NO MAXVALUE reset flags
    "create sequence m99nm as smallint no maxvalue",
    "select max_value from pg_sequences where sequencename = 'm99nm'",
    "create sequence m99nm2 as int no minvalue",
    "select min_value from pg_sequences where sequencename = 'm99nm2'",
    # catalog rename
    "select cycle from pg_sequences where sequencename = 'm99si'",
    "select cycle_option from information_schema.sequences where sequence_name = 'm99si'",
    "select sequence_name, data_type from information_schema.sequences where sequence_name like 'm99%'",
    # correlated derived table (outer_schemas threading)
    "create table m99t(q1 int, q2 int)",
    "insert into m99t values (1, 1), (2, 2)",
    "select *, (select r from (select q1 as q2) x, (select q2 as r) y) from m99t",
    # parse_ident as table function in FROM
    "select * from parse_ident('\"Test\".col')",
    # COST option
    "create function m99c() returns int cost 100 language sql as $$ select 1 $$",
    "select m99c()",
    "create function m99c0() returns int cost 0 language sql as $$ select 1 $$",
    # expression index
    "create table m99e(x int)",
    "create index m99ei on m99e (abs(x))",
    "insert into m99e values (1), (-2)",
    "select * from m99e where abs(x) = 2",
]


def run_phase(log, data_dir, stmts, restart_check_stmts=None):
    errors = []
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

        for sql in stmts:
            err = q(sql)
            if err:
                errors.append((sql, err[:80]))
        q("checkpoint")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    return errors


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc99_")
    log = os.path.join(data_dir, "vg.log")
    errors = run_phase(log, data_dir, STMTS)
    errors += run_phase(log + ".2", data_dir, [
        "select nextval('m99si')",
        "select min_value, max_value from pg_sequences where sequencename = 'm99si'",
        "select cycle from pg_sequences where sequencename = 'm99si'",
        "select count(*) from pg_sequences",
    ])
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


if __name__ == "__main__":
    main()
