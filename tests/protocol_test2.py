#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.2 (extended query protocol).

Drives Parse/Bind/Describe/Execute/Close/Sync by hand over a raw socket and
asserts on the actual wire bytes: ParseComplete ('1'), BindComplete ('2'),
CloseComplete ('3'), ParameterDescription ('t'), RowDescription ('T'),
NoData ('n'), DataRow ('D'), PortalSuspended ('s'), CommandComplete ('C'),
EmptyQueryResponse ('I'), ErrorResponse ('E'), ReadyForQuery ('Z').

Usage: start the server first (`cargo run`), then `python3 tests/protocol_test2.py`.
"""
import socket
import struct
import sys

HOST = "127.0.0.1"
PORT = 5433

passed = []
failed = []


def check(name, cond, detail=""):
    if cond:
        passed.append(name)
        print(f"  ok: {name}")
    else:
        failed.append(name)
        print(f"  FAIL: {name} {detail}")


def read_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise RuntimeError("connection closed by server")
        data += chunk
    return data


def read_msg(sock):
    typ = read_exact(sock, 1)
    (ln,) = struct.unpack("!i", read_exact(sock, 4))
    assert ln >= 4, f"bad message length {ln}"
    payload = read_exact(sock, ln - 4)
    return typ, payload


def cstr(s):
    return s.encode() + b"\x00"


def parse_cstring(payload, pos):
    end = payload.index(b"\x00", pos)
    return payload[pos:end].decode(), end + 1


def parse_rowdesc(payload):
    (nfields,) = struct.unpack("!h", payload[:2])
    pos = 2
    fields = []
    for _ in range(nfields):
        name, pos = parse_cstring(payload, pos)
        table_oid, attnum, type_oid, typlen, typmod, fmt = struct.unpack(
            "!ihihih", payload[pos:pos + 18]
        )
        pos += 18
        fields.append({"name": name, "type_oid": type_oid, "format": fmt})
    return fields


def parse_datarow(payload):
    (ncols,) = struct.unpack("!h", payload[:2])
    pos = 2
    vals = []
    for _ in range(ncols):
        (ln,) = struct.unpack("!i", payload[pos:pos + 4])
        pos += 4
        if ln == -1:
            vals.append(None)
        else:
            vals.append(payload[pos:pos + ln].decode())
            pos += ln
    return vals


def parse_paramdesc(payload):
    (n,) = struct.unpack("!h", payload[:2])
    return list(struct.unpack("!" + "i" * n, payload[2:2 + 4 * n]))


def parse_error(payload):
    fields = {}
    pos = 0
    while payload[pos] != 0:
        code = chr(payload[pos])
        val, pos = parse_cstring(payload, pos + 1)
        fields[code] = val
    return fields


def parse_tag(payload):
    tag, _ = parse_cstring(payload, 0)
    return tag


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        body = struct.pack("!i", 196608) + params
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        # consume handshake through ReadyForQuery
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

    def parse(self, name, query, oids):
        body = cstr(name) + cstr(query) + struct.pack("!h", len(oids))
        for oid in oids:
            body += struct.pack("!i", oid)
        self.send(b"P", body)

    def bind(self, portal, stmt, pformats, params, rformats):
        body = cstr(portal) + cstr(stmt) + struct.pack("!h", len(pformats))
        for f in pformats:
            body += struct.pack("!h", f)
        body += struct.pack("!h", len(params))
        for p in params:
            if p is None:
                body += struct.pack("!i", -1)
            else:
                body += struct.pack("!i", len(p)) + p
        body += struct.pack("!h", len(rformats))
        for f in rformats:
            body += struct.pack("!h", f)
        self.send(b"B", body)

    def describe(self, kind, name):
        self.send(b"D", kind.encode() + cstr(name))

    def execute(self, portal, max_rows):
        self.send(b"E", cstr(portal) + struct.pack("!i", max_rows))

    def close(self, kind, name):
        self.send(b"C", kind.encode() + cstr(name))

    def sync(self):
        self.send(b"S", b"")

    def close_conn(self):
        try:
            self.send(b"X", b"")
        finally:
            self.s.close()


def expect_ready(c):
    t, p = c.recv()
    check("ReadyForQuery idle after Sync", t == b"Z" and p == b"I", f"{t} {p}")


def main():
    print("== setup ==")
    c = Conn()
    msgs = c.query("CREATE TABLE m2items(id INT, name TEXT, price FLOAT8)")
    check("CREATE TABLE", any(t == b"C" and parse_tag(p) == "CREATE TABLE" for t, p in msgs))
    msgs = c.query("INSERT INTO m2items VALUES (1,'hammer',9.5),(2,'nails',1.25),(3,'saw',24.0)")
    check("INSERT 0 3", any(t == b"C" and parse_tag(p) == "INSERT 0 3" for t, p in msgs))

    print("== unnamed SELECT $1 + $2 ==")
    c.parse("", "SELECT $1 + $2", [23, 23])
    t, p = c.recv()
    check("ParseComplete", t == b"1", f"{t}")
    c.describe("S", "")
    t, p = c.recv()
    check("ParameterDescription", t == b"t", f"{t}")
    check("param OIDs [23, 23]", parse_paramdesc(p) == [23, 23], str(parse_paramdesc(p)))
    t, p = c.recv()
    check("RowDescription", t == b"T", f"{t}")
    fields = parse_rowdesc(p)
    check("one ?column? int4 field",
          len(fields) == 1 and fields[0]["name"] == "?column?" and fields[0]["type_oid"] == 23,
          str(fields))
    c.bind("", "", [0], [b"40", b"2"], [0])
    t, p = c.recv()
    check("BindComplete", t == b"2", f"{t}")
    c.execute("", 0)
    t, p = c.recv()
    check("DataRow ['42']", t == b"D" and parse_datarow(p) == ["42"], f"{t} {p}")
    t, p = c.recv()
    check("CommandComplete SELECT 1", t == b"C" and parse_tag(p) == "SELECT 1", f"{t} {p}")
    c.sync()
    expect_ready(c)

    print("== inferred param type (WHERE id = $1, OID 0) ==")
    c.parse("", "SELECT name FROM m2items WHERE id = $1", [0])
    t, p = c.recv()
    check("ParseComplete", t == b"1")
    c.describe("S", "")
    t, p = c.recv()
    check("ParameterDescription infers int4", t == b"t" and parse_paramdesc(p) == [23],
          f"{t} {parse_paramdesc(p) if t == b't' else p}")
    t, p = c.recv()
    fields = parse_rowdesc(p)
    check("RowDescription name/text", t == b"T" and len(fields) == 1
          and fields[0]["name"] == "name" and fields[0]["type_oid"] == 25, str(fields))
    c.bind("", "", [0], [b"2"], [0])
    t, p = c.recv()
    check("BindComplete", t == b"2")
    c.execute("", 0)
    t, p = c.recv()
    check("row is nails", t == b"D" and parse_datarow(p) == ["nails"], f"{t} {p}")
    t, p = c.recv()
    check("CommandComplete SELECT 1", t == b"C" and parse_tag(p) == "SELECT 1")
    c.sync()
    expect_ready(c)

    print("== inferred int for $1 + $2 with OID 0 ==")
    c.parse("", "SELECT $1 + $2", [0, 0])
    t, p = c.recv()
    check("ParseComplete", t == b"1")
    c.describe("S", "")
    t, p = c.recv()
    check("ParameterDescription [23, 23]", t == b"t" and parse_paramdesc(p) == [23, 23],
          str(parse_paramdesc(p) if t == b"t" else p))
    c.recv()  # RowDescription
    c.bind("", "", [0], [b"20", b"22"], [0])
    t, p = c.recv()
    check("BindComplete", t == b"2")
    c.execute("", 0)
    t, p = c.recv()
    check("DataRow ['42']", t == b"D" and parse_datarow(p) == ["42"], f"{t} {p}")
    c.recv()  # CommandComplete
    c.sync()
    expect_ready(c)

    print("== named prepared statement reused across binds ==")
    c.parse("add2", "SELECT $1 + $2", [23, 23])
    check("ParseComplete", c.recv()[0] == b"1")
    c.bind("p1", "add2", [0], [b"40", b"2"], [0])
    check("BindComplete p1", c.recv()[0] == b"2")
    c.execute("p1", 0)
    t, p = c.recv()
    check("p1 -> 42", t == b"D" and parse_datarow(p) == ["42"], f"{p}")
    t, p = c.recv()
    check("p1 CommandComplete SELECT 1", t == b"C" and parse_tag(p) == "SELECT 1", f"{t} {p}")
    c.bind("p2", "add2", [0], [b"7", b"8"], [0])
    check("BindComplete p2", c.recv()[0] == b"2")
    c.execute("p2", 0)
    t, p = c.recv()
    check("p2 -> 15", t == b"D" and parse_datarow(p) == ["15"], f"{p}")
    c.recv()  # CommandComplete
    c.close("S", "add2")
    t, p = c.recv()
    check("CloseComplete", t == b"3", f"{t}")
    c.bind("p3", "add2", [0], [b"1", b"2"], [0])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("Bind on closed statement -> 26000", t == b"E" and err.get("C") == "26000",
          f"{t} {err}")
    c.sync()
    expect_ready(c)

    print("== Execute max-rows suspend/resume ==")
    c.parse("", "SELECT * FROM m2items", [])
    check("ParseComplete", c.recv()[0] == b"1")
    c.bind("", "", [0], [], [0])
    check("BindComplete", c.recv()[0] == b"2")
    c.execute("", 1)
    t, p = c.recv()
    check("row 1", t == b"D" and parse_datarow(p) == ["1", "hammer", "9.5"], f"{p}")
    t, p = c.recv()
    check("PortalSuspended", t == b"s", f"{t}")
    c.execute("", 1)
    t, p = c.recv()
    check("row 2", t == b"D" and parse_datarow(p) == ["2", "nails", "1.25"], f"{p}")
    t, p = c.recv()
    check("PortalSuspended again", t == b"s", f"{t}")
    c.execute("", 0)
    t, p = c.recv()
    check("row 3", t == b"D" and parse_datarow(p) == ["3", "saw", "24"], f"{p}")
    t, p = c.recv()
    check("CommandComplete SELECT 3 (total)", t == b"C" and parse_tag(p) == "SELECT 3",
          f"{t} {p}")
    c.sync()
    expect_ready(c)

    print("== error mid-sequence, then Sync recovers ==")
    c.parse("bad", "SELEC 1", [])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("Parse syntax error 42601", t == b"E" and err.get("C") == "42601", f"{t} {err}")
    # in error state: a Bind is silently discarded (no response bytes)
    c.bind("q", "nope", [0], [], [0])
    c.s.settimeout(0.5)
    try:
        got = c.recv()
        check("message during error state discarded", False, f"got {got[0]}")
    except socket.timeout:
        check("message during error state discarded", True)
    finally:
        c.s.settimeout(10)
    c.sync()
    expect_ready(c)
    # connection still fully usable, both protocols
    msgs = c.query("SELECT 1")
    check("simple query still works", any(t == b"C" and parse_tag(p) == "SELECT 1" for t, p in msgs))
    c.parse("ok1", "SELECT 1", [])
    check("ParseComplete ok1", c.recv()[0] == b"1")
    c.bind("q", "ok1", [0], [b"x"], [0])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("wrong param count -> 08P01", t == b"E" and err.get("C") == "08P01", f"{t} {err}")
    c.sync()
    expect_ready(c)

    print("== type mismatch -> 42804 / bad literal -> 22P02 ==")
    c.parse("tm", "SELECT * FROM m2items WHERE id = $1", [25])
    check("ParseComplete tm", c.recv()[0] == b"1")
    c.bind("p", "tm", [0], [b"2"], [0])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("declared text vs inferred int -> 42804",
          t == b"E" and err.get("C") == "42804", f"{t} {err}")
    c.sync()
    expect_ready(c)
    c.parse("tm2", "SELECT * FROM m2items WHERE id = $1", [23])
    check("ParseComplete tm2", c.recv()[0] == b"1")
    c.bind("p", "tm2", [0], [b"abc"], [0])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("bad int literal -> 22P02", t == b"E" and err.get("C") == "22P02", f"{t} {err}")
    c.sync()
    expect_ready(c)

    print("== binary format -> 0A000 ==")
    c.parse("bf", "SELECT 1", [])
    check("ParseComplete bf", c.recv()[0] == b"1")
    c.bind("p", "bf", [0], [], [1])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("binary result format -> 0A000",
          t == b"E" and err.get("C") == "0A000" and "binary" in err.get("M", ""), f"{t} {err}")
    c.sync()
    expect_ready(c)
    c.bind("p", "bf", [1], [], [0])
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("binary param format -> 0A000",
          t == b"E" and err.get("C") == "0A000", f"{t} {err}")
    c.sync()
    expect_ready(c)

    print("== empty query string ==")
    c.parse("e", "", [])
    check("ParseComplete", c.recv()[0] == b"1")
    c.bind("e", "e", [0], [], [0])
    check("BindComplete", c.recv()[0] == b"2")
    c.execute("e", 0)
    t, p = c.recv()
    check("EmptyQueryResponse", t == b"I", f"{t}")
    c.sync()
    expect_ready(c)

    print("== close portal, then execute -> 34000 ==")
    c.parse("c1", "SELECT 1", [])
    check("ParseComplete c1", c.recv()[0] == b"1")
    c.bind("cp", "c1", [0], [], [0])
    check("BindComplete cp", c.recv()[0] == b"2")
    c.close("P", "cp")
    check("CloseComplete", c.recv()[0] == b"3")
    c.execute("cp", 0)
    t, p = c.recv()
    err = parse_error(p) if t == b"E" else {}
    check("Execute on closed portal -> 34000", t == b"E" and err.get("C") == "34000",
          f"{t} {err}")
    c.sync()
    expect_ready(c)

    print("== describe portal (no ParameterDescription) ==")
    c.parse("dp", "SELECT id, name FROM m2items WHERE id = $1", [0])
    check("ParseComplete dp", c.recv()[0] == b"1")
    c.bind("dpp", "dp", [0], [b"1"], [0])
    check("BindComplete dpp", c.recv()[0] == b"2")
    c.describe("P", "dpp")
    t, p = c.recv()
    fields = parse_rowdesc(p) if t == b"T" else []
    check("RowDescription id/name",
          t == b"T" and [(f["name"], f["type_oid"]) for f in fields] == [("id", 23), ("name", 25)],
          f"{t} {fields}")
    c.execute("dpp", 0)
    t, p = c.recv()
    check("row 1/hammer", t == b"D" and parse_datarow(p) == ["1", "hammer"], f"{p}")
    c.recv()  # CommandComplete
    c.sync()
    expect_ready(c)

    print("== unnamed statement replaced by next Parse('') ==")
    c.parse("", "SELECT 1", [])
    check("ParseComplete #1", c.recv()[0] == b"1")
    c.parse("", "SELECT 2", [])
    check("ParseComplete #2", c.recv()[0] == b"1")
    c.bind("", "", [0], [], [0])
    check("BindComplete", c.recv()[0] == b"2")
    c.execute("", 0)
    t, p = c.recv()
    check("second statement wins -> ['2']", t == b"D" and parse_datarow(p) == ["2"], f"{p}")
    c.recv()  # CommandComplete
    c.sync()
    expect_ready(c)

    print("== NULL param + SELECT $1 ==")
    c.parse("", "SELECT $1", [25])
    check("ParseComplete", c.recv()[0] == b"1")
    c.describe("S", "")
    t, p = c.recv()
    check("ParameterDescription [25]", t == b"t" and parse_paramdesc(p) == [25])
    t, p = c.recv()
    fields = parse_rowdesc(p)
    check("RowDescription ?column?/text", t == b"T" and fields[0]["name"] == "?column?"
          and fields[0]["type_oid"] == 25, str(fields))
    c.bind("", "", [0], [None], [0])
    check("BindComplete", c.recv()[0] == b"2")
    c.execute("", 0)
    t, p = c.recv()
    check("DataRow [None]", t == b"D" and parse_datarow(p) == [None], f"{p}")
    t, p = c.recv()
    check("CommandComplete SELECT 1", t == b"C" and parse_tag(p) == "SELECT 1")
    c.sync()
    expect_ready(c)

    print("== close statement then re-parse ==")
    c.parse("re", "SELECT 41 + $1", [23])
    check("ParseComplete re", c.recv()[0] == b"1")
    c.close("S", "re")
    check("CloseComplete", c.recv()[0] == b"3")
    c.parse("re", "SELECT 41 + $1", [23])
    check("re-ParseComplete", c.recv()[0] == b"1")
    c.bind("", "re", [0], [b"1"], [0])
    check("BindComplete", c.recv()[0] == b"2")
    c.execute("", 0)
    t, p = c.recv()
    check("41 + 1 = 42", t == b"D" and parse_datarow(p) == ["42"], f"{p}")
    c.recv()  # CommandComplete
    c.sync()
    expect_ready(c)

    c.close_conn()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
