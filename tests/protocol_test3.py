#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.3 (transactions).

Drives BEGIN/COMMIT/ROLLBACK/SAVEPOINT over raw sockets and asserts on the
wire bytes, including the ReadyForQuery transaction-status byte:
'I' idle, 'T' in transaction, 'E' in a failed (aborted) transaction.

Usage: start the server first (`cargo run`), then `python3 tests/protocol_test3.py`.
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
        while True:
            t, p = read_msg(self.s)
            if t == b"Z":
                check("handshake ReadyForQuery idle", p == b"I", f"{p!r}")
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

    def sync(self):
        self.send(b"S", b"")

    def close_conn(self):
        try:
            self.send(b"X", b"")
        finally:
            self.s.close()


def split(msgs):
    out = {"rows": [], "tags": [], "error": None, "ready": None}
    for t, p in msgs:
        if t == b"D":
            out["rows"].append(parse_datarow(p))
        elif t == b"C":
            out["tags"].append(parse_tag(p))
        elif t == b"E":
            out["error"] = parse_error(p)
        elif t == b"Z":
            out["ready"] = p
    return out


def main():
    print("== setup ==")
    c1 = Conn()
    r = split(c1.query("CREATE TABLE acct(id INT, bal INT)"))
    check("CREATE TABLE", r["tags"] == ["CREATE TABLE"] and r["ready"] == b"I")
    r = split(c1.query("INSERT INTO acct VALUES (1, 100)"))
    check("INSERT 0 1", r["tags"] == ["INSERT 0 1"])

    print("== BEGIN: own writes visible, others don't see them ==")
    r = split(c1.query("BEGIN"))
    check("BEGIN tag", r["tags"] == ["BEGIN"], str(r["tags"]))
    check("ReadyForQuery 'T'", r["ready"] == b"T", f"{r['ready']!r}")
    r = split(c1.query("INSERT INTO acct VALUES (2, 200)"))
    check("INSERT in txn", r["tags"] == ["INSERT 0 1"])
    check("still 'T'", r["ready"] == b"T")
    r = split(c1.query("SELECT * FROM acct"))
    check("own session sees both rows",
          r["rows"] == [["1", "100"], ["2", "200"]], str(r["rows"]))
    check("SELECT keeps 'T'", r["ready"] == b"T")

    c2 = Conn()
    r = split(c2.query("SELECT * FROM acct"))
    check("other session sees only committed row",
          r["rows"] == [["1", "100"]], str(r["rows"]))
    check("other session idle", r["ready"] == b"I")

    print("== COMMIT publishes ==")
    r = split(c1.query("COMMIT"))
    check("COMMIT tag", r["tags"] == ["COMMIT"], str(r["tags"]))
    check("back to 'I'", r["ready"] == b"I")
    r = split(c2.query("SELECT * FROM acct"))
    check("other session now sees the row",
          r["rows"] == [["1", "100"], ["2", "200"]], str(r["rows"]))

    print("== ROLLBACK discards ==")
    r = split(c1.query("BEGIN"))
    check("BEGIN", r["tags"] == ["BEGIN"] and r["ready"] == b"T")
    r = split(c1.query("INSERT INTO acct VALUES (3, 300)"))
    check("INSERT", r["tags"] == ["INSERT 0 1"])
    r = split(c1.query("ROLLBACK"))
    check("ROLLBACK tag", r["tags"] == ["ROLLBACK"], str(r["tags"]))
    check("back to 'I'", r["ready"] == b"I")
    r = split(c1.query("SELECT * FROM acct"))
    check("rolled-back row is gone",
          r["rows"] == [["1", "100"], ["2", "200"]], str(r["rows"]))

    print("== savepoints ==")
    c1.query("BEGIN")
    c1.query("INSERT INTO acct VALUES (4, 400)")
    r = split(c1.query("SAVEPOINT sp1"))
    check("SAVEPOINT tag", r["tags"] == ["SAVEPOINT"] and r["ready"] == b"T")
    c1.query("INSERT INTO acct VALUES (5, 500)")
    r = split(c1.query("SELECT id FROM acct"))
    check("both rows present", ["4"] in r["rows"] and ["5"] in r["rows"],
          str(r["rows"]))
    r = split(c1.query("ROLLBACK TO SAVEPOINT sp1"))
    check("ROLLBACK TO tag", r["tags"] == ["ROLLBACK"], str(r["tags"]))
    check("still in txn", r["ready"] == b"T")
    r = split(c1.query("SELECT id FROM acct"))
    check("row 4 kept, row 5 rewound",
          ["4"] in r["rows"] and ["5"] not in r["rows"], str(r["rows"]))
    r = split(c1.query("ROLLBACK TO sp1"))
    check("savepoint still valid after ROLLBACK TO", r["error"] is None,
          str(r["error"]))
    r = split(c1.query("RELEASE SAVEPOINT sp1"))
    check("RELEASE tag", r["tags"] == ["RELEASE"], str(r["tags"]))
    r = split(c1.query("ROLLBACK TO sp1"))
    check("released savepoint -> 3B001",
          r["error"] is not None and r["error"].get("C") == "3B001",
          str(r["error"]))
    r = split(c1.query("ROLLBACK"))
    check("ROLLBACK ends txn", r["ready"] == b"I")
    r = split(c1.query("SELECT id FROM acct"))
    check("savepoint-test rows all gone",
          ["4"] not in r["rows"] and ["5"] not in r["rows"], str(r["rows"]))

    print("== failed txn: 25P02 until ROLLBACK ==")
    c1.query("BEGIN")
    r = split(c1.query("INSERT INTO nonexistent VALUES (1)"))
    check("bad INSERT -> 42P01",
          r["error"] is not None and r["error"].get("C") == "42P01",
          str(r["error"]))
    check("ReadyForQuery 'E'", r["ready"] == b"E", f"{r['ready']!r}")
    r = split(c1.query("SELECT 1"))
    check("SELECT in aborted txn -> 25P02",
          r["error"] is not None and r["error"].get("C") == "25P02",
          str(r["error"]))
    check("still 'E'", r["ready"] == b"E")
    r = split(c1.query("COMMIT"))
    check("COMMIT of aborted txn -> ROLLBACK tag", r["tags"] == ["ROLLBACK"],
          str(r["tags"]))
    check("back to 'I'", r["ready"] == b"I")
    r = split(c1.query("SELECT 1"))
    check("connection usable again",
          r["tags"] == ["SELECT 1"] and r["rows"] == [["1"]] and r["ready"] == b"I")

    print("== savepoint recovers from abort ==")
    c1.query("BEGIN")
    c1.query("SAVEPOINT s1")
    r = split(c1.query("INSERT INTO nonexistent VALUES (1)"))
    check("42P01, 'E'", r["error"] is not None and r["ready"] == b"E")
    r = split(c1.query("ROLLBACK TO SAVEPOINT s1"))
    check("ROLLBACK TO recovers", r["tags"] == ["ROLLBACK"] and r["ready"] == b"T",
          f"{r['tags']} {r['ready']!r} {r['error']}")
    r = split(c1.query("INSERT INTO acct VALUES (6, 600)"))
    check("writes work after recovery", r["tags"] == ["INSERT 0 1"])
    r = split(c1.query("COMMIT"))
    check("COMMIT", r["tags"] == ["COMMIT"] and r["ready"] == b"I")
    r = split(c1.query("SELECT id FROM acct WHERE id = 6"))
    check("recovered row committed", r["rows"] == [["6"]], str(r["rows"]))

    print("== implicit txn: each statement atomic ==")
    r = split(c1.query("INSERT INTO acct VALUES (7, 700); INSERT INTO nope VALUES (1)"))
    check("first statement committed", r["tags"] == ["INSERT 0 1"], str(r["tags"]))
    check("second statement errored",
          r["error"] is not None and r["error"].get("C") == "42P01",
          str(r["error"]))
    check("idle after", r["ready"] == b"I")
    r = split(c1.query("SELECT id FROM acct WHERE id = 7"))
    check("first statement's row present", r["rows"] == [["7"]], str(r["rows"]))
    r = split(c1.query("INSERT INTO acct VALUES (8, 800), (9, 'xx')"))
    check("multi-row INSERT with bad row -> 42804",
          r["error"] is not None and r["error"].get("C") == "42804",
          str(r["error"]))
    r = split(c1.query("SELECT id FROM acct WHERE id = 8"))
    check("failed statement left no trace", r["rows"] == [], str(r["rows"]))
    r = split(c1.query("INSERT INTO nope VALUES (1); INSERT INTO acct VALUES (11, 1100)"))
    check("error in first statement", r["error"] is not None, str(r["error"]))
    check("only one tag (rest skipped)", r["tags"] == [], str(r["tags"]))
    r = split(c1.query("SELECT id FROM acct WHERE id = 11"))
    check("second statement never ran", r["rows"] == [], str(r["rows"]))
    r = split(c1.query("INSERT INTO acct VALUES (12, 1200); SELECT bal FROM acct WHERE id = 12"))
    check("two good statements both run",
          r["tags"] == ["INSERT 0 1", "SELECT 1"] and r["rows"] == [["1200"]],
          f"{r['tags']} {r['rows']}")

    print("== DDL rolls back ==")
    c1.query("BEGIN")
    r = split(c1.query("CREATE TABLE temp_t(a INT)"))
    check("CREATE in txn", r["tags"] == ["CREATE TABLE"] and r["ready"] == b"T")
    c1.query("ROLLBACK")
    r = split(c1.query("SELECT * FROM temp_t"))
    check("rolled-back table is gone -> 42P01",
          r["error"] is not None and r["error"].get("C") == "42P01",
          str(r["error"]))
    c1.query("BEGIN")
    c1.query("CREATE TABLE temp_t2(a INT)")
    c1.query("COMMIT")
    r = split(c1.query("SELECT * FROM temp_t2"))
    check("committed table survives", r["tags"] == ["SELECT 0"], str(r["tags"]))
    c1.query("DROP TABLE temp_t2")

    print("== nested BEGIN is a no-op; COMMIT/ROLLBACK outside txn ==")
    r = split(c1.query("BEGIN"))
    check("BEGIN", r["ready"] == b"T")
    r = split(c1.query("BEGIN"))
    check("nested BEGIN no-op, still 'T'",
          r["tags"] == ["BEGIN"] and r["ready"] == b"T", f"{r['tags']} {r['ready']!r}")
    r = split(c1.query("ROLLBACK"))
    check("ROLLBACK", r["ready"] == b"I")
    r = split(c1.query("COMMIT"))
    check("COMMIT outside txn -> COMMIT, 'I'",
          r["tags"] == ["COMMIT"] and r["ready"] == b"I", f"{r['tags']}")
    r = split(c1.query("ROLLBACK"))
    check("ROLLBACK outside txn -> ROLLBACK, 'I'",
          r["tags"] == ["ROLLBACK"] and r["ready"] == b"I")
    r = split(c1.query("SAVEPOINT s"))
    check("SAVEPOINT outside txn -> 25001",
          r["error"] is not None and r["error"].get("C") == "25001",
          str(r["error"]))

    print("== extended protocol inside a transaction ==")
    r = split(c1.query("BEGIN"))
    check("BEGIN", r["ready"] == b"T")
    c1.parse("", "INSERT INTO acct VALUES ($1, $2)", [23, 23])
    check("ParseComplete", c1.recv()[0] == b"1")
    c1.bind("", "", [0], [b"20", b"2000"], [0])
    check("BindComplete", c1.recv()[0] == b"2")
    c1.execute("", 0)
    t, p = c1.recv()
    check("Execute INSERT 0 1", t == b"C" and parse_tag(p) == "INSERT 0 1", f"{t} {p}")
    c1.sync()
    t, p = c1.recv()
    check("Sync -> 'T'", t == b"Z" and p == b"T", f"{t} {p}")
    c1.parse("", "SELECT bal FROM acct WHERE id = $1", [0])
    check("ParseComplete", c1.recv()[0] == b"1")
    c1.describe("S", "")
    t, p = c1.recv()
    check("ParameterDescription infers int4 in txn",
          t == b"t" and parse_paramdesc(p) == [23], f"{t} {p}")
    t, p = c1.recv()
    check("RowDescription", t == b"T", f"{t}")
    c1.bind("", "", [0], [b"20"], [0])
    check("BindComplete", c1.recv()[0] == b"2")
    c1.execute("", 0)
    t, p = c1.recv()
    check("sees own uncommitted row", t == b"D" and parse_datarow(p) == ["2000"],
          f"{t} {p}")
    t, p = c1.recv()
    check("CommandComplete SELECT 1", t == b"C" and parse_tag(p) == "SELECT 1")
    c1.sync()
    t, p = c1.recv()
    check("Sync -> 'T'", t == b"Z" and p == b"T")
    # COMMIT through the extended protocol
    c1.parse("cmt", "COMMIT", [])
    check("ParseComplete COMMIT", c1.recv()[0] == b"1")
    c1.bind("cp", "cmt", [0], [], [0])
    check("BindComplete", c1.recv()[0] == b"2")
    c1.execute("cp", 0)
    t, p = c1.recv()
    check("Execute COMMIT", t == b"C" and parse_tag(p) == "COMMIT", f"{t} {p}")
    c1.sync()
    t, p = c1.recv()
    check("Sync -> 'I' after COMMIT", t == b"Z" and p == b"I", f"{t} {p}")
    r = split(c2.query("SELECT bal FROM acct WHERE id = 20"))
    check("other session sees committed row", r["rows"] == [["2000"]], str(r["rows"]))

    print("== extended: param inference sees txn-created table ==")
    c1.query("BEGIN")
    c1.query("CREATE TABLE p2(id INT)")
    c1.parse("i", "INSERT INTO p2 VALUES ($1)", [0])
    check("ParseComplete", c1.recv()[0] == b"1")
    c1.describe("S", "i")
    t, p = c1.recv()
    check("ParameterDescription [23] from txn working copy",
          t == b"t" and parse_paramdesc(p) == [23], f"{t} {p}")
    t, p = c1.recv()
    check("NoData for INSERT describe", t == b"n", f"{t}")
    c1.sync()
    t, p = c1.recv()
    check("Sync -> 'T'", t == b"Z" and p == b"T")
    c1.query("ROLLBACK")
    r = split(c1.query("SELECT * FROM p2"))
    check("p2 gone after ROLLBACK", r["error"] is not None, str(r["error"]))

    print("== extended: aborted txn recovers via ROLLBACK ==")
    c1.query("BEGIN")
    c1.parse("b", "INSERT INTO nope VALUES (1)", [])
    check("ParseComplete", c1.recv()[0] == b"1")
    c1.bind("bp", "b", [0], [], [0])
    check("BindComplete", c1.recv()[0] == b"2")
    c1.execute("bp", 0)
    t, p = c1.recv()
    err = parse_error(p) if t == b"E" else {}
    check("Execute -> 42P01", t == b"E" and err.get("C") == "42P01", f"{t} {err}")
    c1.sync()
    t, p = c1.recv()
    check("Sync -> 'E'", t == b"Z" and p == b"E", f"{t} {p}")
    c1.parse("r", "SELECT 1", [])
    t, p = c1.recv()
    err = parse_error(p) if t == b"E" else {}
    check("Parse in aborted txn -> 25P02", t == b"E" and err.get("C") == "25P02",
          f"{t} {err}")
    c1.sync()
    c1.recv()  # ReadyForQuery 'E'
    c1.parse("rb", "ROLLBACK", [])
    check("Parse ROLLBACK ok", c1.recv()[0] == b"1")
    c1.bind("rbp", "rb", [0], [], [0])
    check("BindComplete", c1.recv()[0] == b"2")
    c1.execute("rbp", 0)
    t, p = c1.recv()
    check("Execute ROLLBACK", t == b"C" and parse_tag(p) == "ROLLBACK", f"{t} {p}")
    c1.sync()
    t, p = c1.recv()
    check("Sync -> 'I'", t == b"Z" and p == b"I", f"{t} {p}")
    r = split(c1.query("SELECT 1"))
    check("usable again", r["tags"] == ["SELECT 1"] and r["ready"] == b"I")

    c1.close_conn()
    c2.close_conn()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
