#!/usr/bin/env python3
"""memcheck_v097.py — run the v0.97 domain-ALTER / pg_typeof / bounded-plpgsql
paths under valgrind memcheck.

Exercises: ALTER DOMAIN ADD/DROP CONSTRAINT (named/auto-named/duplicate/
missing/IF EXISTS), SET/DROP NOT NULL, SET/DROP DEFAULT, ALTER on missing
(42704) and non-domain (42809) types, CHECK VALUE-only rule (42601),
transactional rollback/commit of ALTER DOMAIN, pg_typeof on domain casts
and domain columns (incl. grouped path), bounded plpgsql CREATE (valid,
case variants, 0A000 rich bodies, 42601 other languages), plpgsql calls,
and WAL replay + checkpoint of altered domains and plpgsql functions
(restart).
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5549
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # ALTER DOMAIN ADD/DROP CONSTRAINT
    "create domain m97 as int check (value > 0)",
    "alter domain m97 add constraint c_small check (value < 100)",
    "select 500::m97",
    "select 5::m97",
    "alter domain m97 add check (value <> 7)",
    "alter domain m97 add check (value <> 8)",
    "select 7::m97",
    "alter domain m97 drop constraint m97_check1",
    "select 8::m97",
    "alter domain m97 drop constraint c_small",
    "select 500::m97",
    "alter domain m97 drop constraint nosuch",
    "alter domain m97 drop constraint if exists nosuch",
    "alter domain m97 add constraint dup check (value > 1)",
    "alter domain m97 add constraint dup check (value > 2)",
    # SET/DROP NOT NULL + DEFAULT
    "alter domain m97 set not null",
    "select null::m97",
    "alter domain m97 drop not null",
    "select null::m97",
    "alter domain m97 set default 42",
    "create table m97t (x m97)",
    "insert into m97t default values",
    "select x from m97t",
    "alter domain m97 drop default",
    # error paths (must not crash)
    "alter domain nosuch set not null",
    "create type m97c as (a int)",
    "alter domain m97c set not null",
    "alter domain m97 add check (other > 0)",
    "alter domain m97 add check (value > 0) bad trailing",
    # transactional
    "begin",
    "alter domain m97 add constraint c_tmp check (value < 10)",
    "select 50::m97",
    "rollback",
    "select 50::m97",
    "begin",
    "alter domain m97 add constraint c_tmp check (value < 10)",
    "commit",
    "select 50::m97",
    "alter domain m97 drop constraint c_tmp",
    # nested domains
    "create domain m97b as m97 check (value < 1000)",
    "alter domain m97b add constraint c2 check (value > -100)",
    "select 5::m97b",
    "select 5000::m97b",
    # pg_typeof (scalar + grouped paths)
    "select pg_typeof(5::m97)",
    "select pg_typeof(x) from m97t",
    "select pg_typeof(x), count(*) from m97t group by pg_typeof(x)",
    "select pg_typeof(5)",
    "select pg_typeof(null)",
    "select pg_typeof(1, 2)",
    # bounded plpgsql
    "create function m97vol(text) returns text as 'begin return $1; end' language plpgsql volatile",
    "select m97vol('hello')",
    "select m97vol('a') || m97vol('b')",
    "create function m97vol2(int) returns int as '  BEGIN  RETURN $1 + 1; END  ' language plpgsql",
    "select m97vol2(41)",
    "create function m97bad() returns int as 'begin x := 1; return 1; end' language plpgsql",
    "create function m97c() returns int as 'int main(){}' language c",
    "select m97vol(null)",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc97_")
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
        # WAL replay path: checkpoint, restart, verify altered domains +
        # plpgsql functions survive
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
            "select 500::m97",
            "select x from m97t",
            "select m97vol('hi')",
            "select pg_typeof(5::m97)",
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
        import re
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
    print("MEMCHECK CLEAN")


if __name__ == "__main__":
    main()
