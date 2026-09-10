#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.1.

Performs the Postgres startup handshake (protocol 3.0) by hand, then drives
the simple query protocol and asserts on the actual wire bytes:
RowDescription ('T'), DataRow ('D'), CommandComplete ('C'), ErrorResponse
('E') and ReadyForQuery ('Z').

Usage: start the server first (`cargo run`), then `python3 tests/protocol_test.py`.
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


def read_until(sock, want):
    msgs = []
    while True:
        t, p = read_msg(sock)
        msgs.append((t, p))
        if t == want:
            return msgs


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
        fields.append(
            {
                "name": name,
                "table_oid": table_oid,
                "attnum": attnum,
                "type_oid": type_oid,
                "typlen": typlen,
                "typmod": typmod,
                "format": fmt,
            }
        )
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


def parse_error(payload):
    fields = {}
    pos = 0
    while payload[pos] != 0:
        code = chr(payload[pos])
        val, pos = parse_cstring(payload, pos + 1)
        fields[code] = val
    return fields


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        body = struct.pack("!i", 196608) + params
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self.handshake = read_until(self.s, b"Z")

    def query(self, sql):
        body = sql.encode() + b"\x00"
        self.s.sendall(b"Q" + struct.pack("!i", len(body) + 4) + body)
        return read_until(self.s, b"Z")

    def close(self):
        try:
            self.s.sendall(b"X" + struct.pack("!i", 4))
        finally:
            self.s.close()


def split(msgs):
    """Split response messages into a dict of parsed pieces."""
    out = {"fields": None, "rows": [], "tag": None, "error": None, "ready": None}
    for t, p in msgs:
        if t == b"T":
            out["fields"] = parse_rowdesc(p)
        elif t == b"D":
            out["rows"].append(parse_datarow(p))
        elif t == b"C":
            tag, _ = parse_cstring(p, 0)
            out["tag"] = tag
        elif t == b"E":
            out["error"] = parse_error(p)
        elif t == b"Z":
            out["ready"] = p
    return out


def main():
    print("== startup handshake ==")
    c = Conn()
    by_type = {}
    for t, p in c.handshake:
        by_type.setdefault(t, []).append(p)

    check("AuthenticationOk received", b"R" in by_type)
    (auth,) = struct.unpack("!i", by_type[b"R"][0][:4])
    check("AuthenticationOk == 0", auth == 0, f"got {auth}")

    params = {}
    for p in by_type.get(b"S", []):
        k, pos = parse_cstring(p, 0)
        v, _ = parse_cstring(p, pos)
        params[k] = v
    check("server_version is 16.0", params.get("server_version") == "16.0", str(params))
    check("server_encoding is UTF8", params.get("server_encoding") == "UTF8")
    check("client_encoding is UTF8", params.get("client_encoding") == "UTF8")
    check("integer_datetimes is on", params.get("integer_datetimes") == "on")

    check(
        "BackendKeyData has pid+secret",
        b"K" in by_type and len(by_type[b"K"][0]) == 8,
    )
    check("ReadyForQuery idle", by_type[b"Z"][0] == b"I")

    print("== SELECT 1 ==")
    r = split(c.query("SELECT 1"))
    check("one field", r["fields"] is not None and len(r["fields"]) == 1)
    check("field name is ?column?", r["fields"][0]["name"] == "?column?")
    check("field type oid is INT4 (23)", r["fields"][0]["type_oid"] == 23)
    check("one row with '1'", r["rows"] == [["1"]], str(r["rows"]))
    check("CommandComplete SELECT 1", r["tag"] == "SELECT 1", str(r["tag"]))
    check("ReadyForQuery idle", r["ready"] == b"I")

    print("== CREATE TABLE ==")
    r = split(c.query("CREATE TABLE users(id INT, name TEXT, active BOOL);"))
    check("CommandComplete CREATE TABLE", r["tag"] == "CREATE TABLE", str(r["tag"]))
    check("no row description", r["fields"] is None)
    check("ReadyForQuery idle", r["ready"] == b"I")

    print("== INSERT ==")
    r = split(c.query("INSERT INTO users VALUES (1,'ada',true),(2,'grace',false);"))
    check("CommandComplete INSERT 0 2", r["tag"] == "INSERT 0 2", str(r["tag"]))

    print("== SELECT * ==")
    r = split(c.query("SELECT * FROM users"))
    names = [f["name"] for f in r["fields"]]
    oids = [f["type_oid"] for f in r["fields"]]
    check("columns id,name,active", names == ["id", "name", "active"], str(names))
    check("oids 23,25,16", oids == [23, 25, 16], str(oids))
    check(
        "two rows, bools as t/f",
        r["rows"] == [["1", "ada", "t"], ["2", "grace", "f"]],
        str(r["rows"]),
    )
    check("CommandComplete SELECT 2", r["tag"] == "SELECT 2", str(r["tag"]))

    print("== SELECT with WHERE ==")
    r = split(c.query("SELECT name FROM users WHERE id = 2"))
    check("one field named name", [f["name"] for f in r["fields"]] == ["name"])
    check("row is grace", r["rows"] == [["grace"]], str(r["rows"]))
    check("CommandComplete SELECT 1", r["tag"] == "SELECT 1")

    print("== SELECT with WHERE + LIMIT ==")
    r = split(c.query("SELECT * FROM users WHERE active = false LIMIT 1"))
    check("one row", r["rows"] == [["2", "grace", "f"]], str(r["rows"]))
    check("CommandComplete SELECT 1", r["tag"] == "SELECT 1")

    print("== string escape ==")
    r = split(c.query("CREATE TABLE t2 (a TEXT)"))
    check("CREATE TABLE", r["tag"] == "CREATE TABLE")
    r = split(c.query("INSERT INTO t2 VALUES ('it''s')"))
    check("INSERT 0 1", r["tag"] == "INSERT 0 1")
    r = split(c.query("SELECT * FROM t2"))
    check("escaped quote round-trips", r["rows"] == [["it's"]], str(r["rows"]))
    r = split(c.query("DROP TABLE t2"))
    check("DROP TABLE", r["tag"] == "DROP TABLE")

    print("== DROP TABLE ==")
    r = split(c.query("DROP TABLE users"))
    check("CommandComplete DROP TABLE", r["tag"] == "DROP TABLE", str(r["tag"]))

    print("== error case: unknown table ==")
    r = split(c.query("SELECT * FROM nope"))
    check("ErrorResponse received", r["error"] is not None)
    check(
        "SQLSTATE 42P01",
        r["error"] is not None and r["error"].get("C") == "42P01",
        str(r["error"]),
    )
    check(
        "message names the table",
        r["error"] is not None and "nope" in r["error"].get("M", ""),
        str(r["error"]),
    )
    check("ReadyForQuery idle after error", r["ready"] == b"I")

    print("== connection survives errors ==")
    r = split(c.query("SELECT 1"))
    check("SELECT 1 still works", r["tag"] == "SELECT 1" and r["rows"] == [["1"]])

    print("== syntax error ==")
    r = split(c.query("SELEC 1"))
    check("ErrorResponse received", r["error"] is not None)
    check(
        "SQLSTATE 42601",
        r["error"] is not None and r["error"].get("C") == "42601",
        str(r["error"]),
    )
    check("ReadyForQuery idle after syntax error", r["ready"] == b"I")

    c.close()

    print(f"\n{len(passed)} passed, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
