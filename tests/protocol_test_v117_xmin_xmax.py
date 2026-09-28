#!/usr/bin/env python3
r"""v1.17 protocol tests: xmin/xmax system columns.

Covers (simple protocol):
- `SELECT xmin, xmax FROM t` -> xmin is the inserting xid (>0),
  xmax is 0 for live rows; RowDescription reports OID 28 (xid)
- `SELECT a.xmin = b.xmin FROM savepoints a, savepoints b WHERE ...`
  (the three conformance statements) -> true
- qualified `a.xmin` / `b.xmin` across different tables pick their own
  range's row
- unqualified `xmin` over a join -> 42702 ambiguous (like PG19)
- user column named xmin shadows the system column
- `SELECT 123::xid` casts, and `xmax` after DELETE stays 0 for survivors

Extended protocol:
- Parse/Describe of an xmin query reports correct field count and
  the xid OID (28).

Self-starting: launches rustgres on 5597 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5597
HERE = os.path.dirname(os.path.abspath(__file__))
SRC_BIN = os.path.join(HERE, "..", "target", "debug", "rustgres")
SCRATCH = os.path.expanduser(
    "~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v117"
)
BIN = os.path.join(SCRATCH, "rg-v117-proto-bin")
DATADIR = os.path.join(SCRATCH, "rg_proto_v117_xmin_xmax")


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
    """Run simple-query SQL, return (nfields, oids, rows, tag, error_code)."""
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

        # --- setup ---
        q(s, "create table savepoints (a int)")
        q(s, "insert into savepoints values (6), (8), (10), (12)")

        # --- xmin/xmax basics + xid OID 28 ---
        nf, oids, rows, tag, err = q(s, "select xmin, xmax from savepoints order by a;")
        ok = (err is None and nf == 2 and oids == [28, 28] and len(rows) == 4
              and all(int(r[0]) > 0 for r in rows) and all(r[1] == "0" for r in rows)
              and len({r[0] for r in rows}) == 1)
        check("xmin>0, xmax=0, OID 28, one inserting xid", ok, (nf, oids, rows, err))

        # --- the three conformance statements ---
        for pred in [("a.a=6", "b.a=8"), ("a.a=10", "b.a=10"), ("a.a=10", "b.a=12")]:
            nf, oids, rows, tag, err = q(
                s, "select a.xmin = b.xmin from savepoints a, savepoints b "
                   "where %s and %s;" % pred)
            check("xmin self-join %s" % (pred,),
                  err is None and rows == [("t",)], (rows, err))

        # --- xmax equality on live rows ---
        nf, oids, rows, tag, err = q(
            s, "select a.xmax = b.xmax from savepoints a, savepoints b "
               "where a.a=6 and b.a=8;")
        check("xmax self-join equality", err is None and rows == [("t",)], (rows, err))

        # --- qualifiers pick their own range across different tables ---
        q(s, "create table other_t (x int)")
        q(s, "insert into other_t values (1)")
        nf, oids, rows, tag, err = q(
            s, "select a.xmin, o.xmin, a.xmin = o.xmin from savepoints a, other_t o "
               "where a.a=8 and o.x=1;")
        ok = (err is None and len(rows) == 1 and int(rows[0][0]) > 0
              and int(rows[0][1]) > 0 and rows[0][0] != rows[0][1]
              and rows[0][2] == "f")
        check("qualified xmin across tables", ok, (rows, err))

        # --- unqualified xmin over a join is ambiguous (42702) ---
        nf, oids, rows, tag, err = q(s, "select xmin from savepoints a, savepoints b limit 1;")
        check("unqualified xmin ambiguous -> 42702", err == "42702", (rows, err))

        # --- user column named xmin shadows the system column ---
        q(s, "create table shadow_x (xmin int)")
        q(s, "insert into shadow_x values (42)")
        nf, oids, rows, tag, err = q(s, "select xmin from shadow_x;")
        check("user xmin column shadows system column",
              err is None and rows == [("42",)] and oids == [23], (rows, oids, err))

        # --- xid cast ---
        nf, oids, rows, tag, err = q(s, "select 123::xid;")
        check("123::xid -> 123", err is None and rows == [("123",)], (rows, err))

        # --- xmax after DELETE: survivors still show 0 ---
        q(s, "delete from savepoints where a = 6;")
        nf, oids, rows, tag, err = q(s, "select a, xmax from savepoints order by a;")
        check("xmax 0 for survivors after delete",
              err is None and rows == [("8", "0"), ("10", "0"), ("12", "0")],
              (rows, err))

        # --- extended protocol Describe: field count + xid OID ---
        nf, oids, err = extended_describe(s, "select xmin, xmax from savepoints")
        check("describe xmin query -> 2 fields, OID 28",
              err is None and nf == 2 and oids == [28, 28], (nf, oids, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.17 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
