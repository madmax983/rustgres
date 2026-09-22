#!/usr/bin/env python3
r"""v0.64 protocol tests: native pg_lsn type metadata (OID 3220).

RED on the inherited base (74491ba: v0.63), GREEN after v0.64.

v0.63's `pg_lsn(numeric)` returned text (OID 25). v0.64 introduces a
native `Value::PgLsn(u64)` / `ColType::PgLsn` so RowDescription reports
the PostgreSQL OID 3220 for pg_lsn, matching PG19's pg_type entry.

- `SELECT pg_lsn(23783416)` → RowDescription type OID 3220, value `0/016AE7F8`.
- `SELECT pg_lsn(0)` → OID 3220, value `0/00000000`.
- `SELECT '0/016AE7F8'::pg_lsn` → OID 3220 (cast input works).
"""
import socket, struct, sys

PORT = 5433

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d

def read_msg(s):
    hdr = read_exact(s, 5)
    typ, ln = struct.unpack("!cI", hdr)
    payload = read_exact(s, ln - 4)
    return typ, payload

def startup(s):
    msg = struct.pack("!II", 196608, 0) + b"user\x00test\x00\x00"
    s.sendall(struct.pack("!I", len(msg) + 4) + msg)
    while True:
        typ, _ = read_msg(s)
        if typ == b"Z":
            break

def query_oid_and_value(s, sql):
    """Run sql, return (type_oid, value_text) for first field of first row."""
    s.sendall(struct.pack("!cI", b"Q", len(sql) + 5) + sql.encode() + b"\x00")
    type_oid = None
    value = None
    while True:
        typ, payload = read_msg(s)
        if typ == b"T":
            nfields = struct.unpack("!H", payload[:2])[0]
            assert nfields == 1, f"expected 1 field, got {nfields}"
            pos = 2
            name_end = payload.find(b"\x00", pos)
            pos = name_end + 1 + 6  # skip name\0, table oid, col attr
            type_oid = struct.unpack("!I", payload[pos:pos+4])[0]
        elif typ == b"D":
            nfields = struct.unpack("!H", payload[:2])[0]
            pos = 2
            ln = struct.unpack("!i", payload[pos:pos+4])[0]
            pos += 4
            value = payload[pos:pos+ln].decode()
        elif typ == b"Z":
            break
        elif typ == b"E":
            raise RuntimeError(f"query failed: {payload}")
    return type_oid, value

def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    try:
        startup(s)
        # pg_lsn(numeric) returns OID 3220
        oid, val = query_oid_and_value(s, "SELECT pg_lsn(23783416)")
        assert oid == 3220, f"pg_lsn(23783416): expected OID 3220, got {oid}"
        assert val == "0/016AE7F8", f"pg_lsn(23783416): expected 0/016AE7F8, got {val}"
        print(f"PASS: pg_lsn(23783416) -> OID {oid}, value {val}")

        oid, val = query_oid_and_value(s, "SELECT pg_lsn(0)")
        assert oid == 3220, f"pg_lsn(0): expected OID 3220, got {oid}"
        assert val == "0/00000000", f"pg_lsn(0): expected 0/00000000, got {val}"
        print(f"PASS: pg_lsn(0) -> OID {oid}, value {val}")

        # Cast from text also yields pg_lsn type
        oid, val = query_oid_and_value(s, "SELECT '0/016AE7F8'::pg_lsn")
        assert oid == 3220, f"cast: expected OID 3220, got {oid}"
        assert val == "0/016AE7F8", f"cast: expected 0/016AE7F8, got {val}"
        print(f"PASS: '0/016AE7F8'::pg_lsn -> OID {oid}, value {val}")

        print("protocol_test65: all pg_lsn OID checks passed")
    finally:
        s.close()

if __name__ == "__main__":
    main()
