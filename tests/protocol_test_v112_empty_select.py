#!/usr/bin/env python3
r"""v1.12 protocol tests: zero-target-list SELECT.

Covers (simple protocol):
- `SELECT;` -> RowDescription with 0 fields, 1 data row, tag SELECT 1
- `SELECT FROM generate_series(1,3);` -> 0 fields, 3 rows
- `SELECT WHERE false;` (v0.87) still works -> 0 fields, 0 rows
- UNION / INTERSECT / EXCEPT over zero-column branches (row counts
  match PG19: union->1, union all->8, intersect->1, intersect all->3,
  except->0, except all->2)
- CTE variants (materialized / not materialized) -> 1 row
- `SELECT FROM;` (no table) still raises 42601

Extended protocol:
- Parse/Describe of `SELECT;` reports 0 fields.
- Parse/Describe of `SELECT FROM generate_series(1,3)` reports 0 fields.

Self-starting: launches rustgres on 5592 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5592
HERE = os.path.dirname(os.path.abspath(__file__))
SRC_BIN = os.path.join(HERE, "..", "target", "debug", "rustgres")
SCRATCH = os.path.expanduser(
    "~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v112"
)
BIN = os.path.join(SCRATCH, "rg-v112-proto-bin")
DATADIR = os.path.join(SCRATCH, "rg_proto_v112_empty_select")


def msg(t, payload):
    return t + struct.pack("!I", 4 + len(payload)) + payload


def cstr(s):
    return s.encode() + b"\x00"


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


def connect():
    for _ in range(30):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
            break
        except ConnectionRefusedError:
            time.sleep(0.5)
    else:
        raise RuntimeError("could not connect")
    body = struct.pack("!I", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!I", len(body) + 4) + body)
    while True:
        t, p = read_msg(s)
        if t == b"Z":
            break
    return s


def q(s, sql):
    """Run simple-query SQL, return (nfields, rows, tag, error_code)."""
    s.sendall(msg(b"Q", cstr(sql)))
    nfields, rows, tag, err = None, [], None, None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            nfields = struct.unpack("!h", p[:2])[0]
        elif t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            j, vals = 2, []
            for _ in range(n):
                ln = struct.unpack("!i", p[j:j+4])[0]; j += 4
                if ln == -1:
                    vals.append(None)
                else:
                    vals.append(p[j:j+ln].decode()); j += ln
            rows.append(tuple(vals))
        elif t == b"C":
            tag = p[:-1].decode()
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return nfields, rows, tag, err


def extended_describe_nfields(s, sql):
    """Parse/Describe via extended protocol; return nfields or (None, err)."""
    s.sendall(msg(b"P", cstr("") + cstr(sql) + struct.pack("!h", 0)))
    s.sendall(msg(b"D", b"S" + cstr("")))
    s.sendall(msg(b"S", b""))
    nfields, err = None, None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            nfields = struct.unpack("!h", p[:2])[0]
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return nfields, err


def main():
    shutil.copy2(SRC_BIN, BIN)
    os.chmod(BIN, 0o755)
    shutil.rmtree(DATADIR, ignore_errors=True)
    os.makedirs(DATADIR)
    srv = subprocess.Popen([BIN, "--data-dir", DATADIR, "--port", str(PORT)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    fails = []
    def check(name, cond, detail=""):
        print(("PASS " if cond else "FAIL ") + name + ((" | " + str(detail)) if detail and not cond else ""))
        if not cond:
            fails.append(name)

    try:
        s = connect()

        # --- bare and FROM-attached empty selects ---
        nf, rows, tag, err = q(s, "select;")
        check("select; -> 0 fields, 1 row", err is None and nf == 0 and len(rows) == 1 and tag == "SELECT 1", (nf, rows, tag, err))
        check("select; rows are empty", err is None and rows == [()], (rows, err))
        nf, rows, tag, err = q(s, "select from generate_series(1,3);")
        check("select from gs(1,3) -> 3 rows", err is None and nf == 0 and len(rows) == 3 and tag == "SELECT 3", (nf, len(rows), tag, err))
        nf, rows, tag, err = q(s, "select where false;")
        check("select where false -> 0 rows", err is None and nf == 0 and rows == [] and tag == "SELECT 0", (nf, rows, tag, err))
        q(s, "create table es_proto (a int)")
        q(s, "insert into es_proto values (1), (2)")
        nf, rows, tag, err = q(s, "select from es_proto;")
        check("select from table -> 2 rows", err is None and nf == 0 and len(rows) == 2, (nf, len(rows), err))
        nf, rows, tag, err = q(s, "select distinct from generate_series(1,3);")
        check("select distinct from -> 1 row", err is None and nf == 0 and len(rows) == 1, (nf, len(rows), err))

        # --- set operations over zero-column branches (PG19 counts) ---
        cases = [
            ("select union select;", 1),
            ("select intersect select;", 1),
            ("select except select;", 0),
            ("select from generate_series(1,5) union select from generate_series(1,3);", 1),
            ("select from generate_series(1,5) union all select from generate_series(1,3);", 8),
            ("select from generate_series(1,5) intersect select from generate_series(1,3);", 1),
            ("select from generate_series(1,5) intersect all select from generate_series(1,3);", 3),
            ("select from generate_series(1,5) except select from generate_series(1,3);", 0),
            ("select from generate_series(1,5) except all select from generate_series(1,3);", 2),
        ]
        for sql, want in cases:
            nf, rows, tag, err = q(s, sql)
            check("setop %d rows: %s" % (want, sql[:45]),
                  err is None and nf == 0 and len(rows) == want,
                  (nf, len(rows), tag, err))

        # --- CTE variants ---
        nf, rows, tag, err = q(s, "with cte as materialized (select s from generate_series(1,5) s) "
                                  "select from cte union select from cte;")
        check("cte materialized -> 1 row", err is None and nf == 0 and len(rows) == 1, (nf, len(rows), err))
        nf, rows, tag, err = q(s, "with cte as not materialized (select s from generate_series(1,5) s) "
                                  "select from cte union select from cte;")
        check("cte not materialized -> 1 row", err is None and nf == 0 and len(rows) == 1, (nf, len(rows), err))

        # --- still errors ---
        nf, rows, tag, err = q(s, "select from;")
        check("select from; still 42601", err == "42601", (nf, rows, tag, err))

        # --- extended protocol Describe ---
        nf, err = extended_describe_nfields(s, "select;")
        check("describe select; -> 0 fields", err is None and nf == 0, (nf, err))
        nf, err = extended_describe_nfields(s, "select from generate_series(1,3)")
        check("describe select from gs -> 0 fields", err is None and nf == 0, (nf, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.12 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
