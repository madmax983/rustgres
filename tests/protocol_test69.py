#!/usr/bin/env python3
r"""v0.68 protocol tests: read-only GUC SQLSTATE + POSIX regex operators.

RED on the v0.67 base (7b16f610), GREEN after v0.68.

1. Read-only GUCs: PG19's set_config_option reports PGC_INTERNAL GUCs
   (server_version, server_version_num) with ERRCODE_CANT_CHANGE_RUNTIME_PARAM
   (55P02, `parameter "..." cannot be changed`) — for SET and for RESET.
   rustgres v0.67 reported 42704 (unrecognized parameter) instead.
   Unknown names are still 42704.
2. Regex match operators `~`, `!~`, `~*`, `!~*` (PG19 textregexeq family):
   parse at the "other native operators" level (syntax.sgml: looser than
   `+`/`-`, tighter than LIKE/BETWEEN/comparisons), evaluate with the
   pure-std engine, NULL propagation, 2201B on invalid patterns, 42883
   on non-text operands. Covers the NAME_TBL conformance slice
   (`~ '.*'`, `!~ '.*'`, `~ '[0-9]'`, `~ '.*asdf.*'`).

Requires a running rustgres server on port 5433.
Run: python3 tests/protocol_test69.py
"""
import socket
import struct
import sys

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
    typ = read_exact(s, 1)
    ln = struct.unpack("!i", read_exact(s, 4))[0]
    payload = read_exact(s, ln - 4)
    return typ, payload


def startup(s):
    msg = struct.pack("!II", 196608, 0) + b"user\x00test\x00\x00"
    s.sendall(struct.pack("!I", len(msg) + 4) + msg)
    while True:
        typ, _ = read_msg(s)
        if typ == b"Z":
            break


def parse_error(payload):
    code, message = None, None
    i = 0
    while i < len(payload) - 1:
        ftype = payload[i : i + 1]
        end = payload.find(b"\x00", i + 1)
        val = payload[i + 1 : end].decode()
        if ftype == b"C":
            code = val
        elif ftype == b"M":
            message = val
        i = end + 1
    return code, message


def run(s, sql):
    """Run one simple-protocol query.

    Returns ("ok", tag, rows) or ("error", code, message). rows is a
    list of first-column text values (None for NULL).
    """
    s.sendall(struct.pack("!cI", b"Q", len(sql) + 5) + sql.encode() + b"\x00")
    tag, rows = None, []
    while True:
        typ, payload = read_msg(s)
        if typ == b"C":
            tag = payload[:-1].decode()
        elif typ == b"D":
            nfields = struct.unpack("!H", payload[:2])[0]
            pos = 2
            ln = struct.unpack("!i", payload[pos : pos + 4])[0]
            pos += 4
            rows.append(payload[pos : pos + ln].decode() if ln >= 0 else None)
        elif typ == b"E":
            code, message = parse_error(payload)
            # drain to ReadyForQuery
            while True:
                t2, _ = read_msg(s)
                if t2 == b"Z":
                    break
            return ("error", code, message)
        elif typ == b"Z":
            break
    return ("ok", tag, rows)


CHECKS = 0


def check(cond, label, detail=""):
    global CHECKS
    CHECKS += 1
    if not cond:
        print(f"FAIL: {label} {detail}")
        raise SystemExit(1)
    print(f"PASS: {label}")


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    try:
        startup(s)

        # --- 1. read-only GUC SQLSTATE ---------------------------------
        for guc in ("server_version", "server_version_num"):
            st, code, msg = run(s, f"SET {guc} = 'bogus'")
            check(
                st == "error" and code == "55P02",
                f"SET {guc} -> 55P02",
                f"st={st} code={code} msg={msg}",
            )
            check(
                msg == f'parameter "{guc}" cannot be changed',
                f"SET {guc} message",
                f"msg={msg!r}",
            )
            st, code, msg = run(s, f"RESET {guc}")
            check(
                st == "error" and code == "55P02",
                f"RESET {guc} -> 55P02",
                f"st={st} code={code}",
            )
        # Unknown GUCs are still 42704.
        st, code, msg = run(s, "SET nosuchguc = 'x'")
        check(
            st == "error" and code == "42704",
            "SET unknown -> 42704",
            f"st={st} code={code}",
        )
        # SHOW still works on the read-only GUCs.
        st, tag, rows = run(s, "SHOW server_version")
        check(st == "ok" and len(rows) == 1, "SHOW server_version works")

        # --- 2. regex operators ----------------------------------------
        cases = [
            ("select 'abc' ~ '.*'", ["t"]),
            ("select 'abc' !~ '.*'", ["f"]),
            ("select 'ABC' ~ 'abc'", ["f"]),
            ("select 'ABC' ~* 'abc'", ["t"]),
            ("select 'ABC' !~* 'abc'", ["f"]),
            ("select 'a1' ~ '[0-9]'", ["t"]),
            ("select 'abc' ~ '[0-9]'", ["f"]),
            ("select 'asdfghjkl;' ~ '.*asdf.*'", ["t"]),
            ("select NULL ~ 'x'", [None]),
            ("select 'x' ~ NULL", [None]),
            # Precedence: ~ tighter than = and LIKE, looser than ||.
            ("select 'ab' ~ '^a' = true", ["t"]),
            ("select 'xay' like 'x%' and 'ab' ~ '^a'", ["t"]),
        ]
        for sql, want in cases:
            st, tag, rows = run(s, sql)
            check(st == "ok" and rows == want, sql, f"got {rows}")

        # Invalid pattern -> 2201B invalid_regular_expression.
        st, code, msg = run(s, "select 'abc' ~ '('")
        check(
            st == "error" and code == "2201B",
            "invalid regex -> 2201B",
            f"st={st} code={code} msg={msg}",
        )
        # Non-text operands -> 42883 undefined_function.
        st, code, msg = run(s, "select 1 ~ 'x'")
        check(
            st == "error" and code == "42883" and "integer ~ text" in msg,
            "int ~ text -> 42883",
            f"st={st} code={code} msg={msg}",
        )
        st, code, msg = run(s, "select 'x' !~* 1")
        check(
            st == "error" and code == "42883" and "text !~* integer" in msg,
            "text !~* int -> 42883",
            f"st={st} code={code} msg={msg}",
        )

        # --- 3. NAME_TBL conformance slice over the wire ----------------
        st, tag, rows = run(s, "create table name_tbl (f1 text)")
        check(st == "ok", "create name_tbl")
        st, tag, rows = run(
            s,
            "insert into name_tbl values "
            "('asdfghjkl;'), ('1234567890ABC'), ('343f%2a'), "
            "('asdfghjkl;asdfghjkl;'), ('1234567890ABC1234567890ABC')",
        )
        check(st == "ok", "populate name_tbl")
        slice_cases = [
            ("select f1 from name_tbl c where c.f1 ~ '.*'", 5),
            ("select f1 from name_tbl c where c.f1 !~ '.*'", 0),
            ("select f1 from name_tbl c where c.f1 ~ '[0-9]'", 3),
            ("select f1 from name_tbl c where c.f1 ~ '.*asdf.*'", 2),
        ]
        for sql, want_n in slice_cases:
            st, tag, rows = run(s, sql)
            check(
                st == "ok" and len(rows) == want_n,
                f"name_tbl slice ({want_n} rows)",
                f"sql={sql} got {len(rows)} rows",
            )
    finally:
        s.close()
    print(f"protocol_test69: {CHECKS} checks passed")


if __name__ == "__main__":
    main()
