#!/usr/bin/env python3
r"""v1.18 protocol tests: index-ordered scans on temporary tables.

Covers (simple protocol):
- CREATE TEMP TABLE + INSERT + CREATE INDEX, then
  `SELECT * FROM t ORDER BY f1` -> rows in ASC order, NULLS LAST,
  served from the session-local temp index (v1.18: the ORDER BY fast
  path used to look the planned index up only in the global map and
  panicked, killing the connection)
- `ORDER BY f1 DESC` -> DESC order, NULLS FIRST
- `ORDER BY f1 LIMIT 2` (early-limit index-order path)
- explicit `NULLS FIRST` on an ASC index -> sort fallback, still correct
- a permanent table with the same shape still uses its own index
  (no cross-talk between temp and global index maps)

Extended protocol:
- Parse/Describe of a temp ORDER BY query reports the right shape.

Self-starting: launches rustgres on 5598 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5598
HERE = os.path.dirname(os.path.abspath(__file__))
SRC_BIN = os.path.join(HERE, "..", "target", "debug", "rustgres")
SCRATCH = os.path.expanduser(
    "~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v118"
)
BIN = os.path.join(SCRATCH, "rg-v118-proto-bin")
DATADIR = os.path.join(SCRATCH, "rg_proto_v118_temp_index_order")


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
    """Run simple-query SQL, return (nfields, oids, rows, tag, error_code).

    Raises RuntimeError("closed") if the server killed the connection
    (the v1.18 bug symptom).
    """
    s.sendall(msg(b"Q", cstr(sql)))
    nfields, oids, rows, tag, err = None, [], [], None, None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            nfields = struct.unpack("!h", p[:2])[0]
            j = 2
            for _ in range(nfields):
                z = p.index(b"\x00", j)
                j = z + 1
                # table OID (4) + attr number (2), then type OID (4)
                oids.append(struct.unpack("!i", p[j + 6:j + 10])[0])
                j += 18
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
    return nfields, oids, rows, tag, err


def extended_describe(s, sql):
    """Parse/Describe via extended protocol; return (nfields, oids) or (None, err)."""
    s.sendall(msg(b"P", cstr("") + cstr(sql) + struct.pack("!h", 0)))
    s.sendall(msg(b"D", b"S" + cstr("")))
    s.sendall(msg(b"S", b""))
    nfields, oids, err = None, [], None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            nfields = struct.unpack("!h", p[:2])[0]
            j = 2
            for _ in range(nfields):
                z = p.index(b"\x00", j)
                j = z + 1
                oids.append(struct.unpack("!i", p[j + 6:j + 10])[0])
                j += 18
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return nfields, oids, err


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

        # --- setup: temp table, data, temp index ---
        for sql in [
            "create temp table t118 (f1 int);",
            "insert into t118 values (42),(3),(10),(7),(null),(null),(1);",
            "create index t118_i on t118 (f1);",
        ]:
            nf, oids, rows, tag, err = q(s, sql)
            check("setup ok: %s" % sql[:40], err is None, (rows, err))

        # --- ASC: index order, NULLS LAST (PG default) ---
        nf, oids, rows, tag, err = q(s, "select * from t118 order by f1;")
        want = [("1",), ("3",), ("7",), ("10",), ("42",), (None,), (None,)]
        check("temp ORDER BY f1 ASC, nulls last (connection survives)",
              err is None and rows == want, (rows, err))

        # --- DESC: index order reversed, NULLS FIRST (PG default) ---
        nf, oids, rows, tag, err = q(s, "select * from t118 order by f1 desc;")
        want = [(None,), (None,), ("42",), ("10",), ("7",), ("3",), ("1",)]
        check("temp ORDER BY f1 DESC, nulls first", err is None and rows == want, (rows, err))

        # --- early-limit path through the same hint ---
        nf, oids, rows, tag, err = q(s, "select * from t118 order by f1 limit 3;")
        check("temp ORDER BY f1 LIMIT 3", err is None and rows == [("1",), ("3",), ("7",)], (rows, err))

        # --- explicit NULLS FIRST on ASC index: hint declines, sort is correct ---
        nf, oids, rows, tag, err = q(s, "select * from t118 order by f1 nulls first;")
        want = [(None,), (None,), ("1",), ("3",), ("7",), ("10",), ("42",)]
        check("temp ORDER BY f1 NULLS FIRST (sort fallback)", err is None and rows == want, (rows, err))

        # --- permanent table with same shape: global index map untouched ---
        for sql in [
            "create table p118 (f1 int);",
            "insert into p118 values (5),(null),(2);",
            "create index p118_i on p118 (f1);",
        ]:
            q(s, sql)
        nf, oids, rows, tag, err = q(s, "select * from p118 order by f1;")
        check("permanent table ORDER BY unaffected",
              err is None and rows == [("2",), ("5",), (None,)], (rows, err))

        # --- temp index invisible to another session (session-local) ---
        s2 = connect()
        nf, oids, rows, tag, err = q(s2, "select * from t118 order by f1;")
        check("temp table invisible to other session -> 42P01", err == "42P01", (rows, err))
        s2.close()

        # --- extended protocol describe ---
        nf, oids, err = extended_describe(s, "select * from t118 order by f1")
        check("extended describe temp ORDER BY", err is None and nf == 1 and oids == [23], (nf, oids, err))

        s.close()
    finally:
        srv.terminate()
        try:
            srv.wait(timeout=10)
        except subprocess.TimeoutExpired:
            srv.kill()

    print("FAILURES: %d" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
