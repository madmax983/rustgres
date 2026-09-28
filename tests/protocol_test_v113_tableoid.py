#!/usr/bin/env python3
r"""v1.13 protocol tests: tableoid system column and pg_size_pretty.

Covers (simple protocol):
- `SELECT tableoid::regclass, * FROM list_parted` -> leaf partition names
- `SELECT tableoid FROM ...` -> numeric OID
- `SELECT tableoid::regclass::text ... GROUP BY 1` -> grouped by leaf
- `pg_size_pretty(8192)` -> '8192 bytes'
- `pg_size_pretty(10240)` -> '10 kB'
- plain table tableoid::regclass -> own table name
- user column named tableoid shadows the system column

Extended protocol:
- Parse/Describe of tableoid query reports correct field count.

Self-starting: launches rustgres on 5593 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5593
HERE = os.path.dirname(os.path.abspath(__file__))
SRC_BIN = os.path.join(HERE, "..", "target", "debug", "rustgres")
SCRATCH = os.path.expanduser(
    "~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v113"
)
BIN = os.path.join(SCRATCH, "rg-v113-proto-bin")
DATADIR = os.path.join(SCRATCH, "rg_proto_v113_tableoid")


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

        # --- setup: partitioned table ---
        q(s, "create table tp (a text, b int) partition by list (a)")
        q(s, "create table tp1 partition of tp for values in ('x')")
        q(s, "create table tp2 partition of tp for values in ('y')")
        q(s, "insert into tp values ('x', 1), ('y', 2)")

        # --- tableoid::regclass names the leaf partition ---
        nf, rows, tag, err = q(s, "select tableoid::regclass, a from tp order by a;")
        check("tableoid::regclass -> leaf names",
              err is None and nf == 2 and len(rows) == 2
              and rows[0][0] == "tp1" and rows[0][1] == "x"
              and rows[1][0] == "tp2" and rows[1][1] == "y",
              (nf, rows, tag, err))

        # --- bare tableoid is a numeric OID ---
        nf, rows, tag, err = q(s, "select tableoid from tp where a = 'x';")
        ok_oid = err is None and len(rows) == 1 and rows[0][0].isdigit() and int(rows[0][0]) > 0
        check("tableoid -> numeric OID", ok_oid, (nf, rows, tag, err))

        # --- tableoid in WHERE ---
        nf, rows, tag, err = q(s, "select a from tp where tableoid::regclass = 'tp1';")
        check("where tableoid::regclass = 'tp1'",
              err is None and rows == [("x",)], (nf, rows, tag, err))

        # --- tableoid in GROUP BY ---
        nf, rows, tag, err = q(s, "select tableoid::regclass::text, count(*) from tp group by 1 order by 1;")
        check("group by tableoid::regclass",
              err is None and len(rows) == 2
              and rows[0][0] == "tp1" and rows[0][1] == "1"
              and rows[1][0] == "tp2" and rows[1][1] == "1",
              (nf, rows, tag, err))

        # --- plain (non-partitioned) table: own name ---
        q(s, "create table plain_t (a int)")
        q(s, "insert into plain_t values (1)")
        nf, rows, tag, err = q(s, "select tableoid::regclass from plain_t;")
        check("plain table tableoid::regclass -> own name",
              err is None and rows == [("plain_t",)], (nf, rows, tag, err))

        # --- user column named tableoid shadows the system column ---
        q(s, "create table shadow_t (tableoid text)")
        q(s, "insert into shadow_t values ('mine')")
        nf, rows, tag, err = q(s, "select tableoid from shadow_t;")
        check("user tableoid column shadows system column",
              err is None and rows == [("mine",)], (nf, rows, tag, err))

        # --- pg_size_pretty ---
        nf, rows, tag, err = q(s, "select pg_size_pretty(8192);")
        check("pg_size_pretty(8192) -> '8192 bytes'",
              err is None and rows == [("8192 bytes",)], (nf, rows, tag, err))
        nf, rows, tag, err = q(s, "select pg_size_pretty(10240);")
        check("pg_size_pretty(10240) -> '10 kB'",
              err is None and rows == [("10 kB",)], (nf, rows, tag, err))
        nf, rows, tag, err = q(s, "select pg_size_pretty(10485760);")
        check("pg_size_pretty(10485760) -> '10 MB'",
              err is None and rows == [("10 MB",)], (nf, rows, tag, err))

        # --- extended protocol Describe ---
        nf, err = extended_describe_nfields(s, "select tableoid::regclass, a from tp")
        check("describe tableoid query -> 2 fields", err is None and nf == 2, (nf, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.13 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
