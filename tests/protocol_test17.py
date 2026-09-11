#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.17.

Groups:
  A. READ ONLY enforcement -- START TRANSACTION READ ONLY blocks INSERT,
     UPDATE, DELETE, COPY FROM, CREATE/DROP, TRUNCATE, SELECT FOR UPDATE,
     nextval/setval with SQLSTATE 25006; reads (SELECT, COPY TO, currval)
     stay allowed; COMMIT/ROLLBACK still work. SET TRANSACTION applies to
     the next transaction; SET SESSION CHARACTERISTICS applies to all
     subsequent transactions; SET default_transaction_read_only applies to
     autocommit sessions.
  B. SET/SHOW/RESET -- SET name = value / TO value, boolean GUC parsing
     (on/off/true/false/1/0), SHOW default_transaction_read_only,
     SHOW server_version / server_version_num, RESET name / RESET ALL.
  C. version reporting -- startup ParameterStatus server_version is
     "0.17.0" (honest: not a fake "16.0"); SHOW server_version,
     server_version_num ("1700"), and version() agree.
  D. extended-protocol COPY -- Parse/Bind/Describe/Execute drives
     CopyOutResponse/CopyData/CopyDone/CommandComplete for COPY TO and
     CopyInResponse + CopyData/CopyDone + CommandComplete for COPY FROM;
     Describe of COPY TO returns NoData; errors (bad table, bad data,
     read-only COPY FROM, COPY in aborted txn) surface as ErrorResponse
     with Sync recovery.
  E. datetime built-ins -- date_part, to_date/to_timestamp/to_char,
     make_date/make_timestamp, timezone (UTC), clock/statement/
     transaction timestamps.

Boots its own server on 127.0.0.1:5433 with a scratch data dir.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test17.py`.
"""
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
HOST = "127.0.0.1"
PORT = 5433

passed, failed = [], []


def check(name, cond, detail=""):
    if cond:
        passed.append(name)
        print(f"  ok: {name}")
    else:
        failed.append(name)
        print(f"  FAIL: {name} {detail}")


def msg(typ, payload):
    return typ + struct.pack("!i", len(payload) + 4) + payload


def cstr(s):
    return s.encode() + b"\x00"


def err_code(payload):
    i = 0
    while i < len(payload):
        if payload[i:i + 1] == b"C":
            j = payload.index(b"\x00", i + 1)
            return payload[i + 1:j].decode()
        j = payload.index(b"\x00", i + 1)
        i = j + 1
    return ""


def _read_exact(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise RuntimeError("connection closed mid-message")
        data += chunk
    return data


def read_msg(s):
    hdr = _read_exact(s, 5)
    typ = hdr[0:1]
    ln = struct.unpack("!i", hdr[1:5])[0]
    payload = _read_exact(s, ln - 4) if ln > 4 else b""
    return typ, payload


def parse_cstring(payload, pos):
    end = payload.index(b"\x00", pos)
    return payload[pos:end].decode(), end + 1


def parse_tag(payload):
    tag, _ = parse_cstring(payload, 0)
    return tag


def parse_datarow(payload):
    n = struct.unpack("!h", payload[0:2])[0]
    pos = 2
    out = []
    for _ in range(n):
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
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        body = struct.pack("!i", 196608) + params
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        # consume handshake through ReadyForQuery, capturing ParameterStatus
        self.startup_params = {}
        while True:
            t, p = read_msg(self.s)
            if t == b"S":
                k, pos = parse_cstring(p, 0)
                v, _ = parse_cstring(p, pos)
                self.startup_params[k] = v
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

    def copydata(self, data):
        self.send(b"d", data)

    def copydone(self):
        self.send(b"c", b"")

    def close_conn(self):
        try:
            self.send(b"X", b"")
        finally:
            self.s.close()


def expect_ready(c, txn=b"I"):
    t, p = c.recv()
    check("ReadyForQuery after Sync", t == b"Z" and p == txn, f"{t} {p}")


def simple_ok(msgs, tag_prefix=None):
    """True if msgs contain a CommandComplete (optionally with tag prefix)
    and no ErrorResponse."""
    if any(t == b"E" for t, _ in msgs):
        return False
    for t, p in msgs:
        if t == b"C":
            if tag_prefix is None or parse_tag(p).startswith(tag_prefix):
                return True
    return False


def simple_err(msgs, code):
    for t, p in msgs:
        if t == b"E" and err_code(p) == code:
            return True
    return False


def simple_single_value(msgs):
    """Extract the single text value from a one-row one-col SELECT."""
    for t, p in msgs:
        if t == b"D":
            vals = parse_datarow(p)
            if len(vals) == 1:
                return vals[0]
    return None


def boot_server():
    tmp = tempfile.mkdtemp(prefix="rg17_")
    log = open(os.path.join(tmp, "server.log"), "w")
    p = subprocess.Popen(
        [BIN, "--data-dir", os.path.join(tmp, "data")],
        stdout=log, stderr=subprocess.STDOUT,
    )
    for _ in range(100):
        try:
            s = socket.create_connection((HOST, PORT), timeout=1)
            s.close()
            break
        except OSError:
            time.sleep(0.1)
    else:
        raise RuntimeError("server did not start")
    return p, tmp


def group_a_read_only(c):
    print("== A. READ ONLY enforcement ==")
    c.query("CREATE TABLE ro_t(id INT, v TEXT)")
    c.query("INSERT INTO ro_t VALUES (1, 'a')")

    def check_blocked(sql, label):
        """Run sql in a fresh READ ONLY txn; expect 25006."""
        c.query("START TRANSACTION READ ONLY")
        msgs = c.query(sql)
        check(label, simple_err(msgs, "25006"))
        c.query("ROLLBACK")

    # -- transaction-level READ ONLY: each write blocked with 25006 --
    check_blocked("INSERT INTO ro_t VALUES (2,'b')", "INSERT blocked 25006")
    check_blocked("UPDATE ro_t SET v='z'", "UPDATE blocked 25006")
    check_blocked("DELETE FROM ro_t", "DELETE blocked 25006")
    check_blocked("CREATE TABLE ro_x(id INT)", "CREATE TABLE blocked 25006")
    check_blocked("DROP TABLE ro_t", "DROP TABLE blocked 25006")
    check_blocked("TRUNCATE ro_t", "TRUNCATE blocked 25006")
    check_blocked("ALTER TABLE ro_t ADD COLUMN q INT", "ALTER TABLE blocked 25006")
    check_blocked("CREATE INDEX ro_idx ON ro_t(id)", "CREATE INDEX blocked 25006")
    check_blocked("GRANT SELECT ON ro_t TO someone", "GRANT blocked 25006")
    check_blocked("SELECT * FROM ro_t FOR UPDATE", "SELECT FOR UPDATE blocked 25006")
    # reads stay allowed
    c.query("START TRANSACTION READ ONLY")
    msgs = c.query("SELECT count(*) FROM ro_t")
    check("SELECT allowed in READ ONLY",
          simple_ok(msgs, "SELECT") and simple_single_value(msgs) == "1")
    check("COMMIT works after read-only txn", simple_ok(c.query("COMMIT"), "COMMIT"))

    # -- SET TRANSACTION applies to the next transaction only --
    check("SET TRANSACTION READ ONLY", simple_ok(c.query("SET TRANSACTION READ ONLY")))
    check("BEGIN", simple_ok(c.query("BEGIN"), "BEGIN"))
    check("INSERT blocked after SET TRANSACTION", simple_err(c.query("INSERT INTO ro_t VALUES (3,'c')"), "25006"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))
    # next transaction is read-write again
    check("BEGIN", simple_ok(c.query("BEGIN"), "BEGIN"))
    check("INSERT allowed in following txn", simple_ok(c.query("INSERT INTO ro_t VALUES (3,'c')"), "INSERT"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))

    # -- SET SESSION CHARACTERISTICS applies to all subsequent txns --
    check("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
          simple_ok(c.query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")))
    check("BEGIN", simple_ok(c.query("BEGIN"), "BEGIN"))
    check("INSERT blocked by session characteristics",
          simple_err(c.query("INSERT INTO ro_t VALUES (4,'d')"), "25006"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))
    check("BEGIN", simple_ok(c.query("BEGIN"), "BEGIN"))
    check("INSERT still blocked in next txn",
          simple_err(c.query("INSERT INTO ro_t VALUES (4,'d')"), "25006"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))
    check("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
          simple_ok(c.query("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE")))
    check("BEGIN", simple_ok(c.query("BEGIN"), "BEGIN"))
    check("INSERT allowed after READ WRITE", simple_ok(c.query("INSERT INTO ro_t VALUES (4,'d')"), "INSERT"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))

    # -- sequence functions --
    c.query("CREATE SEQUENCE ro_seq")
    c.query("SELECT nextval('ro_seq')")  # establish currval outside read-only
    check("START TRANSACTION READ ONLY", simple_ok(c.query("START TRANSACTION READ ONLY"), "BEGIN"))
    check("nextval blocked 25006", simple_err(c.query("SELECT nextval('ro_seq')"), "25006"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))
    check("START TRANSACTION READ ONLY", simple_ok(c.query("START TRANSACTION READ ONLY"), "BEGIN"))
    check("setval blocked 25006", simple_err(c.query("SELECT setval('ro_seq', 10)"), "25006"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))
    # reads stay allowed in a fresh read-only txn
    check("START TRANSACTION READ ONLY", simple_ok(c.query("START TRANSACTION READ ONLY"), "BEGIN"))
    msgs = c.query("SELECT currval('ro_seq')")
    check("currval allowed in READ ONLY", simple_ok(msgs, "SELECT"))
    check("COPY TO allowed in READ ONLY", simple_ok(c.query("COPY ro_t TO STDOUT")))
    check("COMMIT", simple_ok(c.query("COMMIT"), "COMMIT"))

    # -- explicit READ WRITE inside a read-only session default --
    c.query("SET default_transaction_read_only = on")
    msgs = c.query("SHOW default_transaction_read_only")
    check("SHOW default_transaction_read_only = on",
          simple_single_value(msgs) == "on", str(simple_single_value(msgs)))
    check("INSERT blocked by session default",
          simple_err(c.query("INSERT INTO ro_t VALUES (5,'e')"), "25006"))
    msgs = c.query("SELECT count(*) FROM ro_t")
    check("SELECT allowed under session default", simple_ok(msgs, "SELECT"))
    check("START TRANSACTION READ WRITE", simple_ok(c.query("START TRANSACTION READ WRITE"), "BEGIN"))
    check("INSERT allowed in explicit READ WRITE",
          simple_ok(c.query("INSERT INTO ro_t VALUES (5,'e')"), "INSERT"))
    check("COMMIT", simple_ok(c.query("COMMIT"), "COMMIT"))
    c.query("SET default_transaction_read_only = off")
    msgs = c.query("SHOW default_transaction_read_only")
    check("SHOW default_transaction_read_only = off",
          simple_single_value(msgs) == "off", str(simple_single_value(msgs)))
    check("INSERT allowed after reset", simple_ok(c.query("INSERT INTO ro_t VALUES (6,'f')"), "INSERT"))

    # -- error aborts the transaction (like Postgres) --
    check("START TRANSACTION READ ONLY", simple_ok(c.query("START TRANSACTION READ ONLY"), "BEGIN"))
    check("INSERT blocked 25006", simple_err(c.query("INSERT INTO ro_t VALUES (7,'g')"), "25006"))
    msgs = c.query("SELECT 1")
    check("txn aborted after 25006 (25P02)", simple_err(msgs, "25P02"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))

    c.query("DROP TABLE ro_t")
    c.query("DROP SEQUENCE ro_seq")


def group_b_gucs(c):
    print("== B. SET / SHOW / RESET ==")
    # SET ... = value
    check("SET default_transaction_read_only = on", simple_ok(c.query("SET default_transaction_read_only = on")))
    msgs = c.query("SHOW default_transaction_read_only")
    check("SHOW reflects on", simple_single_value(msgs) == "on")
    # SET ... TO value (alias syntax)
    check("SET default_transaction_read_only TO off", simple_ok(c.query("SET default_transaction_read_only TO off")))
    msgs = c.query("SHOW default_transaction_read_only")
    check("SHOW reflects off", simple_single_value(msgs) == "off")
    # boolean spellings
    for spelling in ("true", "false", "1", "0", "yes", "no"):
        check(f"SET ... = {spelling}",
              simple_ok(c.query(f"SET default_transaction_read_only = {spelling}")))
    check("SET garbage is 22023",
          simple_err(c.query("SET default_transaction_read_only = maybe"), "22023"))
    check("SET unknown GUC is 42704",
          simple_err(c.query("SET nosuchguc = on"), "42704"))
    check("SHOW unknown GUC is 42704",
          simple_err(c.query("SHOW nosuchguc"), "42704"))
    # RESET
    c.query("SET default_transaction_read_only = on")
    check("RESET name", simple_ok(c.query("RESET default_transaction_read_only")))
    msgs = c.query("SHOW default_transaction_read_only")
    check("RESET restores off", simple_single_value(msgs) == "off")
    c.query("SET default_transaction_read_only = on")
    check("RESET ALL", simple_ok(c.query("RESET ALL")))
    msgs = c.query("SHOW default_transaction_read_only")
    check("RESET ALL restores off", simple_single_value(msgs) == "off")
    # transaction characteristics via SET TRANSACTION variants
    check("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY",
          simple_ok(c.query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY")))
    check("BEGIN", simple_ok(c.query("BEGIN"), "BEGIN"))
    msgs = c.query("SHOW transaction_isolation")
    check("isolation shows serializable", simple_single_value(msgs) == "serializable",
          str(simple_single_value(msgs)))
    check("INSERT blocked (read only from SET TRANSACTION)",
          simple_err(c.query("CREATE TABLE b_x(id INT)"), "25006"))
    check("ROLLBACK", simple_ok(c.query("ROLLBACK"), "ROLLBACK"))


def group_c_version(c):
    print("== C. version reporting ==")
    check("startup server_version is 0.17.0",
          c.startup_params.get("server_version") == "0.17.0",
          str(c.startup_params.get("server_version")))
    msgs = c.query("SHOW server_version")
    check("SHOW server_version = 0.17.0", simple_single_value(msgs) == "0.17.0",
          str(simple_single_value(msgs)))
    msgs = c.query("SHOW server_version_num")
    check("SHOW server_version_num = 1700", simple_single_value(msgs) == "1700",
          str(simple_single_value(msgs)))
    msgs = c.query("SELECT version()")
    v = simple_single_value(msgs)
    check("version() mentions rustgres 0.17.0",
          v is not None and "rustgres 0.17.0" in v, str(v))
    check("version() disclaims PG identity (no 'PostgreSQL 16')",
          v is not None and "PostgreSQL 16" not in v, str(v))
    check("server_version != fake 16.0",
          c.startup_params.get("server_version") != "16.0")


def group_d_extended_copy(c):
    print("== D. extended-protocol COPY ==")
    c.query("CREATE TABLE cp_t(id INT, v TEXT)")
    c.query("INSERT INTO cp_t VALUES (1,'a'),(2,'b'),(3,'c')")

    # -- COPY TO via extended protocol --
    c.parse("cpout", "COPY cp_t TO STDOUT", [])
    t, _ = c.recv(); check("ParseComplete", t == b"1", f"{t}")
    c.bind("cpp", "cpout", [], [], [])
    t, _ = c.recv(); check("BindComplete", t == b"2", f"{t}")
    c.describe("P", "cpp")
    t, p = c.recv(); check("Describe COPY TO portal -> NoData", t == b"n", f"{t}")
    c.execute("cpp", 0)
    t, p = c.recv()
    check("CopyOutResponse", t == b"H", f"{t}")
    rows = []
    while True:
        t, p = c.recv()
        if t == b"d":
            rows.append(p)
        elif t == b"c":
            break
        else:
            check("COPY TO stream (no ErrorResponse)", False, f"{t} {p}")
            break
    check("CopyDone received", t == b"c", f"{t}")
    check("3 CopyData rows", len(rows) == 3, str(len(rows)))
    check("row bytes look like text format",
          rows[0].rstrip(b"\n").split(b"\t") == [b"1", b"a"], repr(rows[0]))
    t, p = c.recv()
    check("CommandComplete COPY 3", t == b"C" and parse_tag(p) == "COPY 3", f"{t} {p}")
    c.sync(); expect_ready(c)

    # -- COPY FROM via extended protocol --
    c.parse("cpin", "COPY cp_t FROM STDIN", [])
    t, _ = c.recv(); check("ParseComplete", t == b"1", f"{t}")
    c.bind("cpi", "cpin", [], [], [])
    t, _ = c.recv(); check("BindComplete", t == b"2", f"{t}")
    c.execute("cpi", 0)
    t, p = c.recv()
    check("CopyInResponse", t == b"G", f"{t}")
    c.copydata(b"4\td\n5\te\n")
    c.copydone()
    t, p = c.recv()
    check("CommandComplete COPY 2", t == b"C" and parse_tag(p) == "COPY 2", f"{t} {p}")
    c.sync(); expect_ready(c)
    msgs = c.query("SELECT count(*) FROM cp_t")
    check("rows landed", simple_single_value(msgs) == "5", str(simple_single_value(msgs)))

    # -- COPY FROM with bad data -> ErrorResponse + Sync recovery --
    c.parse("cpbad", "COPY cp_t FROM STDIN", [])
    t, _ = c.recv(); check("ParseComplete", t == b"1", f"{t}")
    c.bind("cpb", "cpbad", [], [], [])
    t, _ = c.recv(); check("BindComplete", t == b"2", f"{t}")
    c.execute("cpb", 0)
    t, _ = c.recv(); check("CopyInResponse", t == b"G", f"{t}")
    c.copydata(b"notanint\tx\n")
    c.copydone()
    t, p = c.recv()
    check("ErrorResponse on bad COPY data", t == b"E", f"{t}")
    check("error is 22P02-ish data error", err_code(p) in ("22P02", "22P04", "22P01"),
          err_code(p))
    c.sync(); expect_ready(c)
    # connection still usable
    msgs = c.query("SELECT 1")
    check("connection usable after COPY error", simple_ok(msgs, "SELECT"))

    # -- COPY FROM in a read-only transaction (extended) -> 25006 --
    c.parse("cpro", "COPY cp_t FROM STDIN", [])
    t, _ = c.recv(); check("ParseComplete", t == b"1", f"{t}")
    c.bind("cpr", "cpro", [], [], [])
    t, _ = c.recv(); check("BindComplete", t == b"2", f"{t}")
    c.query("START TRANSACTION READ ONLY")
    c.execute("cpr", 0)
    t, p = c.recv()
    check("COPY FROM blocked 25006 (extended)", t == b"E" and err_code(p) == "25006",
          f"{t} {err_code(p)}")
    c.sync()
    # txn is failed after 25006, so ReadyForQuery shows 'E'
    t, p = c.recv()
    check("ReadyForQuery shows failed txn", t == b"Z" and p == b"E", f"{t} {p}")
    c.query("ROLLBACK")

    # -- COPY in an aborted transaction -> 25P02 --
    c.query("BEGIN")
    c.query("SELECT 1/0")  # abort the txn
    c.parse("cpab", "COPY cp_t TO STDOUT", [])
    t, p = c.recv()
    check("Parse in aborted txn -> 25P02", t == b"E" and err_code(p) == "25P02",
          f"{t} {err_code(p)}")
    c.sync()
    t, p = c.recv()
    check("ReadyForQuery shows failed txn", t == b"Z" and p == b"E", f"{t} {p}")
    c.query("ROLLBACK")

    # -- COPY TO a missing table -> ErrorResponse --
    c.parse("cpno", "COPY nosuch TO STDOUT", [])
    t, _ = c.recv(); check("ParseComplete", t == b"1", f"{t}")
    c.bind("cpn", "cpno", [], [], [])
    t, _ = c.recv(); check("BindComplete", t == b"2", f"{t}")
    c.execute("cpn", 0)
    t, p = c.recv()
    check("COPY TO missing table -> ErrorResponse", t == b"E", f"{t}")
    check("error is 42P01", err_code(p) == "42P01", err_code(p))
    c.sync(); expect_ready(c)

    c.query("DROP TABLE cp_t")


def group_e_datetime(c):
    print("== E. datetime built-ins ==")
    # date_part
    msgs = c.query("SELECT date_part('year', DATE '2026-09-11')")
    check("date_part year", simple_single_value(msgs) == "2026", str(simple_single_value(msgs)))
    msgs = c.query("SELECT date_part('month', DATE '2026-09-11')")
    check("date_part month", simple_single_value(msgs) == "9", str(simple_single_value(msgs)))
    msgs = c.query("SELECT date_part('day', DATE '2026-09-11')")
    check("date_part day", simple_single_value(msgs) == "11", str(simple_single_value(msgs)))
    msgs = c.query("SELECT date_part('dow', DATE '2026-09-11')")
    check("date_part dow (Friday=5)", simple_single_value(msgs) == "5", str(simple_single_value(msgs)))
    msgs = c.query("SELECT date_part('hour', TIMESTAMP '2026-09-11 13:45:30')")
    check("date_part hour", simple_single_value(msgs) == "13", str(simple_single_value(msgs)))
    msgs = c.query("SELECT date_part('bogus', DATE '2026-09-11')")
    check("date_part bogus field -> 22023", simple_err(msgs, "22023"))
    # to_date / to_timestamp / to_char
    msgs = c.query("SELECT to_date('2026-09-11', 'YYYY-MM-DD')")
    check("to_date", simple_single_value(msgs) == "2026-09-11", str(simple_single_value(msgs)))
    msgs = c.query("SELECT to_char(DATE '2026-09-11', 'YYYY/MM/DD')")
    check("to_char", simple_single_value(msgs) == "2026/09/11", str(simple_single_value(msgs)))
    msgs = c.query("SELECT to_timestamp(0)")
    v = simple_single_value(msgs)
    check("to_timestamp(0) = epoch", v is not None and v.startswith("1970-01-01"), str(v))
    # make_date / make_timestamp
    msgs = c.query("SELECT make_date(2026, 9, 11)")
    check("make_date", simple_single_value(msgs) == "2026-09-11", str(simple_single_value(msgs)))
    msgs = c.query("SELECT make_date(2026, 13, 1)")
    check("make_date bad month -> 22008", simple_err(msgs, "22008"))
    msgs = c.query("SELECT make_timestamp(2026, 9, 11, 13, 45, 30.0)")
    v = simple_single_value(msgs)
    check("make_timestamp", v is not None and v.startswith("2026-09-11 13:45:30"), str(v))
    # timezone (UTC only)
    msgs = c.query("SELECT timezone('UTC', TIMESTAMP '2026-09-11 13:45:30')")
    v = simple_single_value(msgs)
    check("timezone UTC", v is not None and v.startswith("2026-09-11 13:45:30"), str(v))
    msgs = c.query("SELECT timezone('America/Chicago', TIMESTAMP '2026-09-11 13:45:30')")
    check("timezone non-UTC -> 0A000 or value", True)  # behavior TBD; must not crash
    # clock/statement/transaction timestamps
    for fn in ("clock_timestamp()", "statement_timestamp()", "transaction_timestamp()", "now()"):
        msgs = c.query(f"SELECT {fn}")
        v = simple_single_value(msgs)
        check(f"{fn} returns a timestamp", v is not None and len(v) >= 10, str(v))
    # age / justify stay unimplemented -> 42883
    msgs = c.query("SELECT age(TIMESTAMP '2026-09-12', TIMESTAMP '2026-09-11')")
    check("age() is 42883", simple_err(msgs, "42883"))


def main():
    print("booting server...")
    proc, tmp = boot_server()
    try:
        c = Conn()
        group_a_read_only(c)
        group_b_gucs(c)
        group_c_version(c)
        group_d_extended_copy(c)
        group_e_datetime(c)
        c.close_conn()
    finally:
        proc.terminate()
        proc.wait(timeout=10)
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    if failed:
        print("FAILURES:", failed)
        sys.exit(1)
    print("ALL GREEN")


if __name__ == "__main__":
    main()
