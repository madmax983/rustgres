#!/usr/bin/env python3
r"""v0.66 protocol tests: SET / SET LOCAL / SHOW / RESET + ALTER SEQUENCE RESTART.

RED on the inherited base (cdd565b: v0.65), GREEN after v0.66.

v0.65 parsed `SET LOCAL` but treated it as session-scoped (the scope
keyword was eaten and ignored), and ALTER SEQUENCE RESTART behaved as
setval(r, true) — the next nextval advanced PAST r.

PG19 behaviors verified here (grounded in the PG docs: SET reference
page and ALTER SEQUENCE reference page):

1. SET LOCAL outside a transaction block -> ERROR 25001
   "SET LOCAL can only be used within a transaction block".
2. SET LOCAL inside a transaction takes effect immediately (SHOW sees
   it) and reverts at COMMIT.
3. SET LOCAL reverts at ROLLBACK too.
4. SET followed by SET LOCAL in one transaction: the LOCAL value is
   seen until COMMIT, then the SET value takes effect.
5. A plain SET inside an aborted (rolled-back) transaction disappears.
6. ROLLBACK TO SAVEPOINT cancels SET LOCAL effects made after the
   savepoint.
7. RESET restores the default; RESET of an unknown GUC is 42704.
8. ALTER SEQUENCE ... RESTART WITH r is setval(r, false): the next
   nextval RETURNS r. Bare RESTART resets to the start value.
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
    list of first-column text values.
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


def check(cond, label):
    if not cond:
        print(f"FAIL: {label}")
        raise SystemExit(1)
    print(f"PASS: {label}")


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    try:
        startup(s)

        # --- baseline ------------------------------------------------
        st, tag, rows = run(s, "SHOW bytea_output")
        check(st == "ok" and rows == ["hex"], f"SHOW bytea_output default -> {rows}")

        # --- session SET persists ------------------------------------
        st, tag, rows = run(s, "SET bytea_output = 'escape'")
        check(st == "ok" and tag == "SET", f"SET tag -> {tag}")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"session SET persists -> {rows}")

        # --- SET LOCAL outside a transaction: 25001 -------------------
        st, code, msg = run(s, "SET LOCAL bytea_output = 'hex'")
        check(
            st == "error"
            and code == "25001"
            and msg == "SET LOCAL can only be used within a transaction block",
            f"SET LOCAL outside txn -> {code} {msg}",
        )
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"value untouched after failed SET LOCAL -> {rows}")

        # --- SET LOCAL reverts at COMMIT ------------------------------
        st, tag, rows = run(s, "BEGIN")
        check(st == "ok", "BEGIN")
        st, tag, rows = run(s, "SET LOCAL bytea_output TO 'hex'")
        check(st == "ok" and tag == "SET", f"SET LOCAL tag -> {tag}")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["hex"], f"SHOW sees LOCAL value in txn -> {rows}")
        st, tag, rows = run(s, "COMMIT")
        check(st == "ok", "COMMIT")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"SET LOCAL reverted at COMMIT -> {rows}")

        # --- SET LOCAL reverts at ROLLBACK ----------------------------
        run(s, "BEGIN")
        run(s, "SET LOCAL bytea_output = 'hex'")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["hex"], f"SHOW sees LOCAL value -> {rows}")
        run(s, "ROLLBACK")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"SET LOCAL reverted at ROLLBACK -> {rows}")

        # --- SET then SET LOCAL: SET wins after COMMIT ----------------
        run(s, "BEGIN")
        run(s, "SET bytea_output = 'hex'")
        run(s, "SET LOCAL bytea_output = 'escape'")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"LOCAL shadows SET in txn -> {rows}")
        run(s, "COMMIT")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["hex"], f"SET value takes effect after COMMIT -> {rows}")
        # restore session baseline for the tests below
        run(s, "SET bytea_output = 'escape'")

        # --- plain SET in an aborted txn disappears -------------------
        run(s, "BEGIN")
        run(s, "SET bytea_output = 'hex'")
        run(s, "ROLLBACK")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"aborted SET disappeared -> {rows}")

        # --- ROLLBACK TO SAVEPOINT cancels later SET LOCAL ------------
        run(s, "BEGIN")
        run(s, "SET LOCAL bytea_output = 'hex'")
        run(s, "SAVEPOINT sp1")
        run(s, "SET LOCAL bytea_output = 'escape'")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"second LOCAL in effect -> {rows}")
        run(s, "ROLLBACK TO sp1")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["hex"], f"post-savepoint LOCAL canceled -> {rows}")
        run(s, "COMMIT")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["escape"], f"surviving LOCAL reverted at COMMIT -> {rows}")

        # --- RESET -----------------------------------------------------
        st, tag, rows = run(s, "RESET bytea_output")
        check(st == "ok" and tag == "RESET", f"RESET tag -> {tag}")
        st, tag, rows = run(s, "SHOW bytea_output")
        check(rows == ["hex"], f"RESET restores default -> {rows}")
        st, code, msg = run(s, "SET no_such_guc = 'x'")
        check(st == "error" and code == "42704", f"unknown GUC -> {code}")
        st, code, msg = run(s, "RESET no_such_guc")
        check(st == "error" and code == "42704", f"RESET unknown GUC -> {code}")
        st, tag, rows = run(s, "SHOW server_version")
        check(st == "ok" and rows == ["0.20.0"], f"SHOW server_version -> {rows}")

        # --- ALTER SEQUENCE RESTART = setval(r, false) -----------------
        st, tag, rows = run(s, "CREATE SEQUENCE rs67")
        check(st == "ok", "CREATE SEQUENCE rs67")
        st, tag, rows = run(s, "SELECT nextval('rs67')")
        check(rows == ["1"], f"nextval first -> {rows}")
        st, tag, rows = run(s, "ALTER SEQUENCE rs67 RESTART WITH 100")
        check(st == "ok" and tag == "ALTER SEQUENCE", f"ALTER SEQUENCE tag -> {tag}")
        st, tag, rows = run(s, "SELECT nextval('rs67')")
        check(rows == ["100"], f"nextval after RESTART WITH 100 returns 100 -> {rows}")
        st, tag, rows = run(s, "SELECT nextval('rs67')")
        check(rows == ["101"], f"nextval advances after -> {rows}")
        run(s, "ALTER SEQUENCE rs67 RESTART")
        st, tag, rows = run(s, "SELECT nextval('rs67')")
        check(rows == ["1"], f"bare RESTART resets to start -> {rows}")
        run(s, "DROP SEQUENCE rs67")

        print("protocol_test67: all SET/LOCAL/SHOW/RESET/RESTART checks passed")
    finally:
        s.close()


if __name__ == "__main__":
    main()
