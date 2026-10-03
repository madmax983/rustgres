#!/usr/bin/env python3
r"""v1.14 protocol tests: SELECT ... FOR UPDATE OF tbl [, ...].

Covers (simple protocol):
- `SELECT ... FOR UPDATE OF t` on a plain table -> rows returned
- `SELECT ... FOR UPDATE OF a, b` multi-target -> rows returned
- `FOR UPDATE OF` with a join alias -> ERROR 0A000
  ("FOR UPDATE cannot be applied to a join")
- `FOR UPDATE OF` with an unknown name -> ERROR 42P01
  ("relation ... not found in FROM clause")
- `FOR UPDATE OF` with a subquery alias -> rows returned
- bare `FOR UPDATE` still works (no OF list)

Extended protocol:
- Parse/Describe of a FOR UPDATE OF query reports correct field count.

Self-starting: launches rustgres on 5594 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5594
HERE = os.path.dirname(os.path.abspath(__file__))
SRC_BIN = os.path.join(HERE, "..", "target", "debug", "rustgres")
SCRATCH = os.path.expanduser(
    "~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v114"
)
BIN = os.path.join(SCRATCH, "rg-v114-proto-bin")
DATADIR = os.path.join(SCRATCH, "rg_proto_v114_for_update_of")


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
    s.sendall(msg(b"Q", cstr(sql)))
    nfields, rows, tag, err = 0, [], None, None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            nfields = struct.unpack("!h", p[:2])[0]
        elif t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            off, row = 2, []
            for _ in range(n):
                ln = struct.unpack("!i", p[off:off + 4])[0]
                off += 4
                if ln < 0:
                    row.append(None)
                else:
                    row.append(p[off:off + ln].decode())
                    off += ln
            rows.append(tuple(row))
        elif t == b"C":
            tag = p[:-1].decode()
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
            m2 = re.search(rb"M([^\x00]*)", p)
            msg_text = m2.group(1).decode() if m2 else ""
            err = (err, msg_text)
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

        # --- setup ---
        q(s, "create table fuo_t1 (a int, b int)")
        q(s, "create table fuo_t2 (a int, c int)")
        q(s, "insert into fuo_t1 values (1, 10), (2, 20)")
        q(s, "insert into fuo_t2 values (1, 100), (3, 300)")

        # --- FOR UPDATE OF single table ---
        nf, rows, tag, err = q(s, "select fuo_t1.a from fuo_t1, fuo_t2 where fuo_t1.a = fuo_t2.a for update of fuo_t1;")
        check("FOR UPDATE OF single table -> rows",
              err is None and rows == [("1",)], (nf, rows, tag, err))

        # --- FOR UPDATE OF with alias ---
        nf, rows, tag, err = q(s, "select x.a from fuo_t1 as x for update of x;")
        check("FOR UPDATE OF alias -> rows",
              err is None and len(rows) == 2, (nf, rows, tag, err))

        # --- FOR UPDATE OF multiple tables ---
        nf, rows, tag, err = q(s, "select fuo_t1.a from fuo_t1, fuo_t2 for update of fuo_t1, fuo_t2;")
        check("FOR UPDATE OF multi -> rows",
              err is None and len(rows) == 4, (nf, rows, tag, err))

        # --- FOR UPDATE OF subquery alias (PG allows) ---
        nf, rows, tag, err = q(s, "select * from (select a from fuo_t1) as sq, fuo_t2 for update of sq;")
        check("FOR UPDATE OF subquery alias -> rows",
              err is None and len(rows) == 4, (nf, rows, tag, err))

        # --- FOR UPDATE OF join alias -> 0A000 ---
        nf, rows, tag, err = q(s, "select * from fuo_t1 join fuo_t2 using (a) as j for update of j;")
        ok = err is not None and err[0] == "0A000" and "cannot be applied to a join" in err[1]
        check("FOR UPDATE OF join alias -> 0A000", ok, (nf, rows, tag, err))

        # --- FOR UPDATE OF unknown name -> 42P01 ---
        nf, rows, tag, err = q(s, "select * from fuo_t1 for update of nope;")
        ok = err is not None and err[0] == "42P01" and "not found in FROM clause" in err[1]
        check("FOR UPDATE OF unknown -> 42P01", ok, (nf, rows, tag, err))

        # --- bare FOR UPDATE still works ---
        nf, rows, tag, err = q(s, "select a from fuo_t1 for update;")
        check("bare FOR UPDATE -> rows",
              err is None and len(rows) == 2, (nf, rows, tag, err))

        # --- extended protocol Describe ---
        nf, err = extended_describe_nfields(s, "select a from fuo_t1 for update of fuo_t1")
        check("describe FOR UPDATE OF query -> 1 field", err is None and nf == 1, (nf, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.14 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
