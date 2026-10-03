"""v1.15 protocol test: quote_ident / quote_literal / quote_nullable
(PG19 quote.c semantics over the wire, simple + extended protocol)."""
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
BIN = os.path.join(TMP, "rustgres-proto-v115")
PORT = 5545
DATADIR = os.path.join(TMP, "proto-v115-data")


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
            err = m.group(1).decode() if m else "?????"
            m2 = re.search(rb"M([^\x00]*)", p)
            msg_text = m2.group(1).decode() if m2 else ""
            err = (err, msg_text)
        elif t == b"Z":
            break
    return nfields, rows, tag, err


def extended_q(s, sql):
    """Parse/Bind/Describe/Execute via extended protocol; return rows, err."""
    s.sendall(msg(b"P", cstr("") + cstr(sql) + struct.pack("!h", 0)))
    s.sendall(msg(b"B", cstr("") + cstr("") + struct.pack("!h", 0)
                  + struct.pack("!h", 0) + struct.pack("!h", 0)))
    s.sendall(msg(b"D", b"S" + cstr("")))
    s.sendall(msg(b"E", cstr("") + struct.pack("!i", 0)))
    s.sendall(msg(b"S", b""))
    rows, err = [], None
    while True:
        t, p = read_msg(s)
        if t == b"D":
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
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return rows, err


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

        # --- quote_literal basics ---
        nf, rows, tag, err = q(s, "select quote_literal('abc');")
        check("quote_literal plain -> 'abc'",
              err is None and rows == [("'abc'",)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_literal('');")
        check("quote_literal empty -> ''",
              err is None and rows == [("''",)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_literal('O''Brien');")
        check("quote_literal embedded quote doubled",
              err is None and rows == [("'O''Brien'",)], (nf, rows, tag, err))

        # --- quote_literal E'' syntax on backslash ---
        nf, rows, tag, err = q(s, r"select quote_literal('a\b');")
        check("quote_literal backslash -> E'a\\\\b'",
              err is None and rows == [("E'a\\\\b'",)], (nf, rows, tag, err))

        # --- quote_literal strict on NULL ---
        nf, rows, tag, err = q(s, "select quote_literal(NULL);")
        check("quote_literal(NULL) -> NULL",
              err is None and rows == [(None,)], (nf, rows, tag, err))

        # --- quote_ident safe shapes ---
        nf, rows, tag, err = q(s, "select quote_ident('abc');")
        check("quote_ident safe -> abc",
              err is None and rows == [("abc",)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_ident('_x1');")
        check("quote_ident underscore -> _x1",
              err is None and rows == [("_x1",)], (nf, rows, tag, err))

        # unreserved keyword stays bare
        nf, rows, tag, err = q(s, "select quote_ident('abort');")
        check("quote_ident unreserved kw -> abort",
              err is None and rows == [("abort",)], (nf, rows, tag, err))

        # --- quote_ident quoting ---
        nf, rows, tag, err = q(s, "select quote_ident('select');")
        check("quote_ident reserved kw -> \"select\"",
              err is None and rows == [('"select"',)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_ident('My Col');")
        check("quote_ident space/upper -> \"My Col\"",
              err is None and rows == [('"My Col"',)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, 'select quote_ident(\'a"b\');')
        check("quote_ident embedded quote doubled",
              err is None and rows == [('"a""b"',)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_ident(NULL);")
        check("quote_ident(NULL) -> NULL",
              err is None and rows == [(None,)], (nf, rows, tag, err))

        # --- quote_nullable ---
        nf, rows, tag, err = q(s, "select quote_nullable(NULL);")
        check("quote_nullable(NULL) -> 'NULL'",
              err is None and rows == [("NULL",)], (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_nullable('x');")
        check("quote_nullable('x') -> 'x'",
              err is None and rows == [("'x'",)], (nf, rows, tag, err))

        # --- wrong arity -> 42883 ---
        nf, rows, tag, err = q(s, "select quote_literal('a', 'b');")
        check("quote_literal 2 args -> 42883",
              err is not None and err[0] == "42883", (nf, rows, tag, err))

        nf, rows, tag, err = q(s, "select quote_ident();")
        check("quote_ident 0 args -> 42883",
              err is not None and err[0] == "42883", (nf, rows, tag, err))

        # --- extended protocol ---
        rows, err = extended_q(s, "select quote_literal('it''s')")
        check("extended quote_literal",
              err is None and rows == [("'it''s'",)], (rows, err))

        rows, err = extended_q(s, "select quote_ident('between')")
        check("extended quote_ident keyword",
              err is None and rows == [('"between"',)], (rows, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.15 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
