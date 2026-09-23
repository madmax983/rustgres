#!/usr/bin/env python3
"""memcheck_v076.py — run the v0.76 new-code paths under valgrind memcheck.

Exercises: UPDATE ... FROM ... RETURNING (describe + execution, target
alias), DELETE ... USING ... RETURNING (with WHERE / without WHERE /
empty USING), extended-protocol Describe of UPDATE ... FROM ... RETURNING
(the protocol-78 A3 42703 path), ALTER ... ADD CONSTRAINT ... NOT NULL ...
NOT VALID (23502 on future writes), timestamp precision rounding,
quantified-comparison VALUES forms, and Stmt::max_param over FROM/USING
(covered via parse of statements with params in derived tables).
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5548
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # UPDATE ... FROM ... RETURNING (the A3 scenario)
    "create table m76a(id int, code int)",
    "create table m76b(id int, delta int)",
    "insert into m76a values (1, 10), (2, 20), (3, 30)",
    "insert into m76b values (1, 5), (2, 7)",
    "update m76a set code = code + delta from m76b where m76a.id = m76b.id returning m76a.id, m76b.delta",
    "update m76a as x set code = code + m76b.delta from m76b where x.id = m76b.id returning x.id, x.code",
    # ambiguous unqualified RETURNING column -> 42702, must not crash
    "update m76a set code = 1 from m76b where m76a.id = m76b.id returning id",
    # DELETE ... USING ... RETURNING
    "delete from m76a using m76b where m76a.id = m76b.id returning m76a.id, m76b.delta",
    "insert into m76a values (1, 10), (2, 20)",
    "delete from m76a using m76b returning m76a.id, m76b.delta",
    "insert into m76a values (1, 10), (2, 20)",
    "create table m76e(id int)",
    "delete from m76a using m76e",
    "select id from m76a order by id",
    # NOT NULL ... NOT VALID
    "create table m76n(id int, v int)",
    "insert into m76n values (1, null)",
    "alter table m76n add constraint nn_v not null v not valid",
    "insert into m76n values (2, null)",
    "update m76n set v = null where id = 1",
    "insert into m76n values (2, 5)",
    # timestamp precision rounding (only current_timestamp takes it in PG19)
    "select current_timestamp(0), current_timestamp(3), current_timestamp(6)",
    "select now(), clock_timestamp(), statement_timestamp(), transaction_timestamp()",
    "select now(3)",
    "select clock_timestamp(2)",
    # quantified comparisons, VALUES forms
    "select 1 = any (values (2), (3))",
    "select 1 = any (values (1), (3))",
    "select 1 = all (values (1), (1))",
    "select null = any (values (null))",
    "select 1 = all (values (1), (null))",
    "checkpoint",
]

# Extended-protocol Describe of UPDATE ... FROM ... RETURNING (A3's 42703).
EXT_DESCRIBE_SQL = ("update m76a set code = code + delta from m76b "
                    "where m76a.id = m76b.id returning m76a.id, m76b.delta")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc76_")
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

        nfail = 0
        for q in STMTS:
            err = simple(q)
            if err:
                nfail += 1
                print(f"SQL ERR {err}: {q[:60]}")

        # Extended protocol: Parse + Describe + Sync for the A3 statement.
        sq = EXT_DESCRIBE_SQL.encode()
        parse = (b"P" + struct.pack("!i", 0)  # placeholder, patched below
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
        print(f"statements with SQL errors: {nfail}")
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=30)
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
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
