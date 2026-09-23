#!/usr/bin/env python3
"""protocol_test79.py — v0.77 protocol coverage.

v0.77 changes under test:
A. Simple-protocol parse errors fail an explicit transaction (PG: any
   error aborts the txn). Before v0.77 the txn stayed usable after a
   parse error; now the session reports 25P02 until ROLLBACK, and
   ReadyForQuery shows 'E' (in failed transaction). RED on v0.76.
B. Extended-protocol parameters inside FROM/USING items bind and
   execute end-to-end: a $n inside a FROM derived table of an UPDATE,
   a $n inside a USING derived table of a DELETE, and a $n inside a
   JOIN derived table of a SELECT. (v0.76 implemented the substitution
   and unit-tested max_param sizing, but had no execution coverage;
   this group is green on v0.76 too and guards the behavior.)

This test manages its own server on port 5551 and is RED on the v0.76
base (group A), GREEN on the v0.77 branch.
"""
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5551
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = 0
failed = 0


def check(name, cond, extra=""):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name} {extra}")


def cstr(s):
    return s.encode() + b"\x00"


def read_msg(s):
    typ = s.recv(1)
    ln = struct.unpack("!i", s.recv(4))[0]
    payload = b""
    while len(payload) < ln - 4:
        chunk = s.recv(ln - 4 - len(payload))
        if not chunk:
            raise RuntimeError("connection closed")
        payload += chunk
    return typ, payload


def parse_fields(payload):
    """Parse an ErrorResponse/NoticeResponse payload into {field: value}."""
    out = {}
    pos = 0
    while pos < len(payload) - 1:
        kind = payload[pos:pos + 1]
        end = payload.index(b"\x00", pos + 1)
        out[kind] = payload[pos + 1:end].decode(errors="replace")
        pos = end + 1
    return out


def err_code(msgs):
    for t, p in msgs:
        if t == b"E":
            return parse_fields(p).get(b"C")
    return None


def ready_status(msgs):
    for t, p in msgs:
        if t == b"Z":
            return p
    return None


def parse_datarow(payload):
    out = []
    nfields = struct.unpack("!h", payload[:2])[0]
    pos = 2
    for _ in range(nfields):
        ln = struct.unpack("!i", payload[pos:pos + 4])[0]
        pos += 4
        if ln == -1:
            out.append(None)
        else:
            out.append(payload[pos:pos + ln].decode())
            pos += ln
    return out


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = read_msg(self.s)
            if t == b"Z":
                break

    def send(self, typ, body):
        self.s.sendall(typ + struct.pack("!i", len(body) + 4) + body)

    def recv(self):
        return read_msg(self.s)

    def query(self, sql):
        """Simple-protocol query; returns messages up to ReadyForQuery."""
        self.send(b"Q", cstr(sql))
        msgs = []
        while True:
            t, p = self.recv()
            msgs.append((t, p))
            if t == b"Z":
                return msgs

    def parse(self, name, query, oids=()):
        body = cstr(name) + cstr(query) + struct.pack("!h", len(oids))
        for oid in oids:
            body += struct.pack("!i", oid)
        self.send(b"P", body)

    def bind(self, portal, stmt, params, pformats=(), rformats=(0,)):
        pf = pformats if pformats else (0,) * len(params)
        body = cstr(portal) + cstr(stmt) + struct.pack("!h", len(pf))
        for f in pf:
            body += struct.pack("!h", f)
        body += struct.pack("!h", len(params))
        for p in params:
            pb = p.encode() if isinstance(p, str) else p
            body += struct.pack("!i", len(pb)) + pb
        body += struct.pack("!h", len(rformats))
        for f in rformats:
            body += struct.pack("!h", f)
        self.send(b"B", body)

    def execute(self, portal, max_rows=0):
        self.send(b"E", cstr(portal) + struct.pack("!i", max_rows))

    def sync(self):
        self.send(b"S", b"")

    def ext_round(self, name, sql, params):
        """Parse/Bind/Execute/Sync one parameterized statement; returns
        (tag, rows, err_code, ready_status)."""
        self.parse(name, sql)
        t, _ = self.recv()
        if t != b"1":
            return None, [], "parse-failed", None
        self.bind("p_" + name, name, params)
        t, _ = self.recv()
        if t != b"2":
            return None, [], "bind-failed", None
        self.execute("p_" + name)
        self.sync()
        tag, rows, ecode = None, [], None
        while True:
            t, p = self.recv()
            if t == b"T":
                pass
            elif t == b"D":
                rows.append(parse_datarow(p))
            elif t == b"C":
                tag = p[:-1].decode()
            elif t == b"E":
                ecode = parse_fields(p).get(b"C")
            elif t == b"Z":
                return tag, rows, ecode, p
        # unreachable

    def close_conn(self):
        try:
            self.send(b"X", b"")
        finally:
            self.s.close()


def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError("server did not start")


def stop_server(proc):
    try:
        proc.terminate()
        proc.wait(timeout=10)
    except Exception:
        proc.kill()
    time.sleep(1.0)


def main():
    data_dir = tempfile.mkdtemp(prefix="rg79_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # ---- A. simple-protocol parse error fails an explicit txn ----
        r = c.query("BEGIN")
        check("A1 BEGIN ok", err_code(r) is None, str(err_code(r)))
        check("A2 BEGIN -> in txn", ready_status(r) == b"T",
              str(ready_status(r)))
        r = c.query("SELEC oops FROM nowhere")
        check("A3 parse error code 42601", err_code(r) == "42601",
              str(err_code(r)))
        check("A4 ReadyForQuery shows failed txn (E)",
              ready_status(r) == b"E", str(ready_status(r)))
        r = c.query("SELECT 1")
        check("A5 next statement -> 25P02", err_code(r) == "25P02",
              str(err_code(r)))
        check("A6 still failed txn", ready_status(r) == b"E",
              str(ready_status(r)))
        r = c.query("SELECT 2")
        check("A7 second statement also 25P02", err_code(r) == "25P02",
              str(err_code(r)))
        r = c.query("ROLLBACK")
        check("A8 ROLLBACK ok", err_code(r) is None, str(err_code(r)))
        check("A9 back to idle", ready_status(r) == b"I",
              str(ready_status(r)))
        r = c.query("SELECT 42")
        check("A10 SELECT works after ROLLBACK", err_code(r) is None,
              str(err_code(r)))
        # Outside a txn, a parse error poisons nothing.
        r = c.query("SELEC oops FROM nowhere")
        check("A11 parse error outside txn is 42601", err_code(r) == "42601",
              str(err_code(r)))
        check("A12 idle after", ready_status(r) == b"I",
              str(ready_status(r)))
        r = c.query("SELECT 7")
        check("A13 next statement fine", err_code(r) is None,
              str(err_code(r)))

        # ---- B. extended-protocol params inside FROM/USING items ----
        r = c.query("CREATE TABLE p79a(id int, v int)")
        check("B0 create", err_code(r) is None, str(err_code(r)))
        r = c.query("INSERT INTO p79a VALUES (1, 10), (2, 20)")
        check("B0 insert", err_code(r) is None, str(err_code(r)))

        tag, rows, ecode, st = c.ext_round(
            "upd79",
            "UPDATE p79a SET v = v + s.d FROM (SELECT $1 AS d) s "
            "WHERE p79a.id = $2",
            ["5", "1"])
        check("B1 UPDATE..FROM with $1 in FROM item executes",
              ecode is None and tag == "UPDATE 1", f"{tag} {ecode}")
        check("B2 txn still idle", st == b"I", str(st))
        r = c.query("SELECT v FROM p79a WHERE id = 1")
        val = None
        for t, p in r:
            if t == b"D":
                val = parse_datarow(p)[0]
        check("B3 param applied (10 + 5 = 15)", val == "15", str(val))

        tag, rows, ecode, st = c.ext_round(
            "del79",
            "DELETE FROM p79a USING (SELECT $1 AS did) s "
            "WHERE p79a.id = s.did",
            ["2"])
        check("B4 DELETE..USING with $1 in USING item executes",
              ecode is None and tag == "DELETE 1", f"{tag} {ecode}")
        r = c.query("SELECT id FROM p79a ORDER BY id")
        ids = [parse_datarow(p)[0] for t, p in r if t == b"D"]
        check("B5 row id=2 deleted", ids == ["1"], str(ids))

        tag, rows, ecode, st = c.ext_round(
            "sel79",
            "SELECT a.id FROM p79a a JOIN (SELECT $1 AS x) s ON a.id = s.x",
            ["1"])
        check("B6 SELECT with $1 in JOIN item executes", ecode is None,
              str(ecode))
        check("B7 JOIN param row correct", rows == [["1"]], str(rows))

        c.close_conn()
    finally:
        stop_server(proc)
    shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol_test79: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
