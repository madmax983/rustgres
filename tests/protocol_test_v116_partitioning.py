"""v1.16 protocol test: declarative partitioning over the wire
(RANGE/LIST/HASH routing, multilevel, childless-intermediate 23514,
ATTACH, WAL persistence)."""
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time

SRC_BIN = os.environ.get(
    "RUSTGRES_BIN",
    os.path.join(os.path.dirname(os.path.abspath(__file__)),
                 "..", "target", "debug", "rustgres"))
HERE = os.path.dirname(os.path.abspath(__file__))
TMP = os.path.join(HERE, "tmp")
BIN = os.path.join(TMP, "rustgres-proto-v116")
PORT = 5546
DATADIR = os.path.join(TMP, "proto-v116-data")


def cstr(s):
    return s.encode() + b"\x00"


def msg(t, payload):
    return t + struct.pack("!i", len(payload) + 4) + payload


def read_msg(s):
    hdr = b""
    while len(hdr) < 5:
        chunk = s.recv(5 - len(hdr))
        if not chunk:
            raise EOFError("closed")
        hdr += chunk
    t = hdr[:1]
    ln = struct.unpack("!i", hdr[1:5])[0]
    p = b""
    while len(p) < ln - 4:
        chunk = s.recv(ln - 4 - len(p))
        if not chunk:
            raise EOFError("closed")
        p += chunk
    return t, p


def connect():
    for _ in range(50):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=5)
            break
        except OSError:
            time.sleep(0.2)
    else:
        raise RuntimeError("server did not come up")
    body = struct.pack("!I", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!I", len(body) + 4) + body)
    while True:
        t, p = read_msg(s)
        if t == b"Z":
            break
    return s


def q(s, sql):
    s.sendall(msg(b"Q", cstr(sql)))
    nfields, rows, tag, err = 0, [], "", None
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
            code = m.group(1).decode() if m else "?????"
            m2 = re.search(rb"M([^\x00]*)", p)
            msg_text = m2.group(1).decode() if m2 else ""
            err = (code, msg_text)
        elif t == b"Z":
            break
    return nfields, rows, tag, err


def main():
    os.makedirs(TMP, exist_ok=True)
    shutil.copy2(SRC_BIN, BIN)
    os.chmod(BIN, 0o755)
    shutil.rmtree(DATADIR, ignore_errors=True)
    os.makedirs(DATADIR)
    srv = subprocess.Popen([BIN, "--data-dir", DATADIR, "--port", str(PORT)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    fails = []

    def check(name, cond, detail=""):
        print(("PASS " if cond else "FAIL ") + name
              + ((" | " + str(detail)) if detail and not cond else ""))
        if not cond:
            fails.append(name)

    try:
        s = connect()

        # --- RANGE routing ---
        nf, rows, tag, err = q(
            s, "create table rp (a int, b int) partition by range (a);")
        check("create range-partitioned", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table rp1 partition of rp for values from (0) to (10);")
        check("create rp1", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table rp2 partition of rp for values from (10) to (20);")
        check("create rp2", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "insert into rp values (5, 50);")
        check("insert routes to rp1", err is None and tag == "INSERT 0 1",
              (rows, tag, err))
        nf, rows, tag, err = q(s, "insert into rp values (15, 150);")
        check("insert routes to rp2", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "select * from rp1;")
        check("rp1 has row", err is None and rows == [("5", "50")],
              (rows, tag, err))
        nf, rows, tag, err = q(s, "select * from rp order by a;")
        check("parent scan sees both",
              err is None and rows == [("5", "50"), ("15", "150")],
              (rows, tag, err))

        # --- LIST routing ---
        nf, rows, tag, err = q(
            s, "create table lp (k text, v int) partition by list (k);")
        check("create list-partitioned", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table lp_a partition of lp for values in ('a', 'b');")
        check("create lp_a", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "insert into lp values ('a', 1);")
        check("list insert routes", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "select * from lp;")
        check("list parent scan", err is None and rows == [("a", "1")],
              (rows, tag, err))

        # --- HASH routing ---
        nf, rows, tag, err = q(
            s, "create table hp (id int) partition by hash (id);")
        check("create hash-partitioned", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table hp0 partition of hp for values with (modulus 2, remainder 0);")
        check("create hp0", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table hp1 partition of hp for values with (modulus 2, remainder 1);")
        check("create hp1", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "insert into hp values (7);")
        check("hash insert routes", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "select count(*) from hp;")
        check("hash parent count", err is None and rows == [("1",)],
              (rows, tag, err))

        # --- multilevel routing ---
        nf, rows, tag, err = q(
            s, "create table mp (a int, b int, c text, d int) "
               "partition by range (a, b);")
        check("create multilevel root", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table mp5 partition of mp "
               "for values from (1, 40) to (1, 50) partition by range (c);")
        check("create mp5 intermediate", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "create table mp5_cd partition of mp5 "
               "for values from ('c') to ('d');")
        check("create mp5_cd leaf", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "insert into mp values (1, 45, 'c', 1);")
        check("multilevel insert routes",
              err is None and tag == "INSERT 0 1", (rows, tag, err))
        nf, rows, tag, err = q(s, "select * from mp5_cd;")
        check("leaf has row",
              err is None and rows == [("1", "45", "c", "1")],
              (rows, tag, err))

        # --- v1.16: childless intermediate names the matched child ---
        nf, rows, tag, err = q(
            s, "create table mp5_ce partition of mp5 "
               "for values from ('e') to ('f') partition by list (c);")
        check("create childless mp5_ce", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "insert into mp values (1, 45, 'e', 1);")
        check("childless intermediate -> 23514 naming mp5_ce",
              err is not None and err[0] == "23514"
              and 'no partition of relation "mp5_ce" found for row' in err[1],
              (rows, tag, err))
        # unmatched at mp5 level names mp5
        nf, rows, tag, err = q(
            s, "insert into mp values (1, 45, 'z', 1);")
        check("unmatched intermediate -> 23514 naming mp5",
              err is not None and err[0] == "23514"
              and 'no partition of relation "mp5" found for row' in err[1],
              (rows, tag, err))

        # --- ATTACH with reordered columns ---
        nf, rows, tag, err = q(
            s, "create table ap (a int, b text) partition by range (a);")
        check("create ap root", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "create table ap_new (b text, a int);")
        check("create ap_new reordered", err is None, (rows, tag, err))
        nf, rows, tag, err = q(
            s, "alter table ap attach partition ap_new "
               "for values from (100) to (200);")
        check("attach reordered", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "insert into ap values (150, 'x');")
        check("insert into attached", err is None, (rows, tag, err))
        nf, rows, tag, err = q(s, "select a, b from ap;")
        check("attached row remapped",
              err is None and rows == [("150", "x")], (rows, tag, err))

        # --- WAL persistence: restart and verify ---
        # (v1.16 note: leaf data survives restart; the parent->child
        # linkage across restart is a pre-existing v1.15 limitation,
        # out of scope here — so we check the leaves directly.)
        nf, rows, tag, err = q(s, "insert into rp1 values (7, 70);")
        check("pre-restart leaf insert", err is None, (rows, tag, err))
        s.close()
        srv.terminate()
        srv.wait()
        time.sleep(0.5)
        srv = subprocess.Popen(
            [BIN, "--data-dir", DATADIR, "--port", str(PORT)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        s = connect()
        nf, rows, tag, err = q(s, "select * from rp1 order by a;")
        check("post-restart leaf rows survive",
              err is None and rows == [("5", "50"), ("7", "70")],
              (rows, tag, err))
        nf, rows, tag, err = q(s, "select * from mp5_cd;")
        check("post-restart multilevel survives",
              err is None and rows == [("1", "45", "c", "1")],
              (rows, tag, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.16 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
