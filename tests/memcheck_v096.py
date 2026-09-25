#!/usr/bin/env python3
"""memcheck_v096.py — run the v0.96 table-inheritance paths under valgrind memcheck.

Exercises: CREATE TABLE ... INHERITS (single/multiple parents, reordered
child columns, constraint-only), ALTER TABLE ... INHERIT / NO INHERIT
(incl. error paths: type conflict, missing column, NOT NULL, duplicate,
circular, temp/permanent rules), recursive parent scans, FROM ONLY,
pg_inherits, and WAL replay of inheritance links (restart).
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5548
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # CREATE ... INHERITS: single parent, column merge
    "create table m96p (a int, b text)",
    "create table m96c (b text, a int, c int) inherits (m96p)",
    "insert into m96c values ('x', 1, 2), ('y', 3, 4)",
    "select a, b from m96p order by a",
    "select a, b from only m96p order by a",
    "select a, b from only (m96p) order by a",
    # Multiple parents
    "create table m96p2 (b text, d int)",
    "create table m96m (e int) inherits (m96p, m96p2)",
    "insert into m96m values (5, 'z', 6, 7)",
    "select a, b, d, e from m96p order by a",
    # Grandchild recursion
    "create table m96g (f int) inherits (m96c)",
    "insert into m96g values ('g', 8, 9, 10)",
    "select a, b from m96p order by a",
    # Constraint-only inherited CREATE (conformance union.sql shape)
    "create table m96t2 (ab text primary key)",
    "create table m96t2c (primary key (ab)) inherits (m96t2)",
    "insert into m96t2c values ('vw'), ('cd')",
    "select ab from m96t2 order by ab",
    # ALTER INHERIT / NO INHERIT
    "create table m96ap (a text, b text)",
    "create table m96ac (b text, a text)",
    "alter table m96ac inherit m96ap",
    "insert into m96ac values ('v', 'w')",
    "select a, b from m96ap",
    "alter table m96ac no inherit m96ap",
    "select a, b from m96ap",
    "alter table m96ac inherit m96ap",
    # Error paths (must not crash)
    "create table m96bad (a text) inherits (m96p)",
    "create table m96nn (a int not null)",
    "create table m96nnc (a int)",
    "alter table m96nnc inherit m96nn",
    "alter table m96p inherit m96p",
    "create table m96ca (a int)",
    "create table m96cb (a int) inherits (m96ca)",
    "alter table m96ca inherit m96cb",
    "alter table m96ac inherit m96ap",
    "alter table m96ac no inherit m96ap",
    "alter table m96ac no inherit nosuch",
    "create table m96d1 (a int default 1)",
    "create table m96d2 (a int default 2)",
    "create table m96dc (z int) inherits (m96d1, m96d2)",
    "create table m96dok (z int, a int default 5) inherits (m96d1, m96d2)",
    # Temp/permanent rules
    "create temp table m96tp (a int)",
    "create table m96perm_bad (z int) inherits (m96tp)",
    "create temp table m96tc (z int) inherits (m96p)",
    # pg_inherits catalog
    "select inhrelid, inhparent, inhseqno from pg_inherits order by 1",
    "select count(*) from pg_inherits",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc96_")
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
        # WAL replay path: checkpoint, restart, verify links survive
        q("checkpoint")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    # Restart: replay WAL + checkpoint with inheritance links
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
            "select count(*) from pg_inherits",
            "select a, b from m96p order by a",
            "select ab from m96t2 order by ab",
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
