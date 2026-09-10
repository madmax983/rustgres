#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.13 (replication foundations).

Groups:
  A. regular clients unaffected — normal SQL works with and without the
     replication startup parameter present-but-false; replication=database
     is a normal connection.
  B. replication negotiation — IDENTIFY_SYSTEM exact shape (systemid,
     timeline, xlogpos, dbname); system id stable across connections and
     restarts; non-superuser replication connection refused (42501);
     unknown replication command -> 42601; TIMELINE_HISTORY/BASE_BACKUP
     -> 0A000.
  C. slot lifecycle — CREATE (exact row), duplicate -> 42710, bad name ->
     42602, bad plugin -> 0A000, DROP, drop-missing -> 42704, drop-active
     -> 55006, pg_replication_slots exact shape, slots survive SIGKILL.
  D. logical streaming — START_REPLICATION gives CopyBothResponse, then
     exact BEGIN/INSERT/UPDATE/DELETE/COMMIT lines (old+new values),
     DDL markers, keepalive framing, standby-status flush tracking via
     pg_replication_slots, active flag set/cleared.
  E. error pins — START on missing slot -> 42704, on physical slot ->
     0A000, bad LSN -> 42602.

Each group boots fresh servers on scratch ports/data dirs.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test13.py`.
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

_next_port = [56143]

def alloc_port():
    _next_port[0] += 1
    return _next_port[0]

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

def parse_lsn(s):
    hi, lo = s.split("/")
    return (int(hi, 16) << 32) | int(lo, 16)

class Conn:
    """Normal SQL connection (simple protocol)."""
    def __init__(self, port, user="postgres", timeout=30, extra_params=None):
        self.s = socket.create_connection((HOST, port), timeout=timeout)
        body = struct.pack("!i", 196608)
        body += b"user\x00" + user.encode() + b"\x00"
        for k, v in (extra_params or {}).items():
            body += k.encode() + b"\x00" + v.encode() + b"\x00"
        body += b"\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self._drain_until_ready()

    def _read_msg(self, timeout=30):
        self.s.settimeout(timeout)
        t = self.s.recv(1)
        if not t:
            raise RuntimeError("connection closed by server")
        (ln,) = struct.unpack("!i", _read_exact(self.s, 4))
        return t, _read_exact(self.s, ln - 4)

    def _drain_until_ready(self):
        while True:
            t, p = self._read_msg()
            if t == b"Z":
                return
            if t == b"E":
                raise RuntimeError("auth failed: " + err_code(p))

    def q(self, sql):
        self.s.sendall(msg(b"Q", cstr(sql)))
        tags, rows, codes, descs = [], [], [], []
        while True:
            t, p = self._read_msg()
            if t == b"T":
                (n,) = struct.unpack("!h", p[:2])
                pos, cols = 2, []
                for _ in range(n):
                    j = p.index(b"\x00", pos)
                    cols.append(p[pos:j].decode())
                    pos = j + 1 + 18
                descs.append(tuple(cols))
            elif t == b"C":
                tags.append(p[:-1].decode())
            elif t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    if ln == -1:
                        r.append(None)
                    else:
                        r.append(p[pos:pos + ln].decode())
                        pos += ln
                rows.append(tuple(r))
            elif t == b"E":
                codes.append(err_code(p))
            elif t == b"Z":
                break
        return tags, rows, codes, descs

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        except OSError:
            pass
        self.s.close()

class ReplConn(Conn):
    """Replication connection: startup with replication=true."""
    def __init__(self, port, user="postgres", database="postgres", timeout=30):
        self.s = socket.create_connection((HOST, port), timeout=timeout)
        body = struct.pack("!i", 196608)
        body += b"user\x00" + user.encode() + b"\x00"
        body += b"database\x00" + database.encode() + b"\x00"
        body += b"replication\x00true\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self._drain_until_ready()

    def repl_query(self, sql):
        """One replication command; returns (tags, rows, codes, descs)."""
        return self.q(sql)

    def start_replication(self, slot, lsn="0/0"):
        self.s.sendall(msg(b"Q", cstr(f"START_REPLICATION SLOT {slot} LOGICAL {lsn}")))
        t, p = self._read_msg(timeout=10)
        assert t == b"W", f"expected CopyBothResponse, got {t}"
        return Streamer(self.s)

class Streamer:
    """Reads the CopyBoth stream after START_REPLICATION."""
    def __init__(self, s):
        self.s = s
        self.lines = []  # decoded logical lines, in order

    def _read_msg(self, timeout):
        self.s.settimeout(timeout)
        t = self.s.recv(1)
        if not t:
            raise RuntimeError("connection closed by server")
        (ln,) = struct.unpack("!i", _read_exact(self.s, 4))
        return t, _read_exact(self.s, ln - 4)

    def poll(self, timeout=5):
        """Read until timeout; returns (xlog_lines, keepalives)."""
        xlog, keep = [], []
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                t, p = self._read_msg(timeout=max(0.1, deadline - time.time()))
            except (socket.timeout, TimeoutError):
                break
            if t != b"d":
                raise RuntimeError(f"expected CopyData, got {t}")
            kind = p[0:1]
            if kind == b"d":
                data_start, wal_end, _st = struct.unpack("!qqq", p[1:25])
                text = p[25:].decode()
                xlog.append((data_start, wal_end, text))
                self.lines.append(text)
            elif kind == b"w":
                wal_end, _st, reply = struct.unpack("!qqB", p[1:18])
                keep.append((wal_end, reply))
            else:
                raise RuntimeError(f"unknown CopyData payload {kind}")
        return xlog, keep

    def wait_for_lines(self, count, timeout=15):
        deadline = time.time() + timeout
        while len(self.lines) < count and time.time() < deadline:
            self.poll(timeout=min(2, max(0.1, deadline - time.time())))
        return self.lines

    def standby_status(self, flushed_lsn):
        """"I have flushed everything up to flushed_lsn (int)."""
        now = int((time.time() + 946684800) * 1_000_000)
        payload = b"r" + struct.pack("!qqqqB", flushed_lsn, flushed_lsn,
                                     flushed_lsn, now, 0)
        self.s.sendall(msg(b"d", payload))

    def copy_done(self):
        self.s.sendall(msg(b"c", b""))
        self.s.settimeout(None)

class Server:
    def __init__(self):
        self.port = alloc_port()
        self.datadir = tempfile.mkdtemp(prefix="rg13-")
        self.proc = subprocess.Popen(
            [BIN, "--port", str(self.port), "--data-dir", self.datadir],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                socket.create_connection((HOST, self.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        shutil.rmtree(self.datadir, ignore_errors=True)

    def kill9(self):
        self.proc.kill()
        self.proc.wait(timeout=10)

    def restart(self):
        self.proc = subprocess.Popen(
            [BIN, "--port", str(self.port), "--data-dir", self.datadir],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                socket.create_connection((HOST, self.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)

def alive(port):
    try:
        c = Conn(port)
        _, rows, codes, _ = c.q("SELECT 1")
        c.close()
        return not codes and rows == [("1",)]
    except Exception:
        return False

# ---------------------------------------------------------------------------
# A. regular clients unaffected
# ---------------------------------------------------------------------------

def t_regular_clients_unaffected():
    srv = Server()
    try:
        c = Conn(srv.port)
        tags, rows, codes, _ = c.q("CREATE TABLE a_reg(id INT PRIMARY KEY, v TEXT)")
        check("a-regular-create", not codes and tags == ["CREATE TABLE"], f"{codes}")
        _, _, codes, _ = c.q("INSERT INTO a_reg VALUES (1, 'x'), (2, 'y')")
        check("a-regular-insert", not codes, f"{codes}")
        _, rows, codes, _ = c.q("SELECT id, v FROM a_reg ORDER BY id")
        check("a-regular-select", not codes and rows == [("1", "x"), ("2", "y")],
              f"{codes} {rows}")
        _, _, codes, _ = c.q("UPDATE a_reg SET v='z' WHERE id=1")
        check("a-regular-update", not codes, f"{codes}")
        _, _, codes, _ = c.q("DELETE FROM a_reg WHERE id=2")
        check("a-regular-delete", not codes, f"{codes}")
        _, rows, codes, _ = c.q("SELECT COUNT(*) FROM a_reg")
        check("a-regular-count", not codes and rows == [("1",)], f"{codes} {rows}")
        c.close()
        # replication=database is a normal SQL connection.
        c2 = Conn(srv.port, extra_params={"replication": "database"})
        _, rows, codes, _ = c2.q("SELECT COUNT(*) FROM a_reg")
        check("a-replication-database-is-normal", not codes and rows == [("1",)],
              f"{codes} {rows}")
        c2.close()
        check("a-server-alive", alive(srv.port))
    finally:
        srv.stop()

# ---------------------------------------------------------------------------
# B. replication negotiation
# ---------------------------------------------------------------------------

def t_identify_system():
    srv = Server()
    try:
        r = ReplConn(srv.port)
        tags, rows, codes, descs = r.repl_query("IDENTIFY_SYSTEM")
        check("b-ident-shape",
              not codes and descs == [("systemid", "timeline", "xlogpos", "dbname")],
              f"{codes} {descs}")
        check("b-ident-tag", tags == ["IDENTIFY_SYSTEM"], f"{tags}")
        sysid, timeline, xlogpos, dbname = rows[0]
        check("b-ident-timeline", timeline == "1", f"{timeline}")
        check("b-ident-dbname", dbname == "postgres", f"{dbname}")
        check("b-ident-sysid-numeric", sysid.isdigit() and int(sysid) != 0, f"{sysid}")
        lsn = parse_lsn(xlogpos)
        check("b-ident-xlogpos", lsn >= 0, f"{xlogpos}")
        # System id is stable across connections.
        r2 = ReplConn(srv.port)
        _, rows2, _, _ = r2.repl_query("IDENTIFY_SYSTEM")
        check("b-ident-stable-conn", rows2[0][0] == sysid, f"{rows2[0][0]} != {sysid}")
        r.close(); r2.close()
        # ... and across restarts (separate system.id file).
        srv.kill9(); srv.restart()
        r3 = ReplConn(srv.port)
        _, rows3, _, _ = r3.repl_query("IDENTIFY_SYSTEM")
        check("b-ident-stable-restart", rows3[0][0] == sysid, f"{rows3[0][0]} != {sysid}")
        r3.close()
    finally:
        srv.stop()

def t_replication_requires_superuser():
    srv = Server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE repluser LOGIN")
        c.close()
        try:
            ReplConn(srv.port, user="repluser")
            check("b-nonsuperuser-refused", False, "connection accepted")
        except RuntimeError as e:
            check("b-nonsuperuser-refused", "42501" in str(e), str(e)[:120])
        check("b-server-alive", alive(srv.port))
    finally:
        srv.stop()

def t_replication_command_errors():
    srv = Server()
    try:
        r = ReplConn(srv.port)
        _, _, codes, _ = r.repl_query("SELECT 1")
        check("b-nonrepl-command-42601", codes == ["42601"], f"{codes}")
        _, _, codes, _ = r.repl_query("TIMELINE_HISTORY 1")
        check("b-timeline-history-0A000", codes == ["0A000"], f"{codes}")
        _, _, codes, _ = r.repl_query("BASE_BACKUP LABEL 'x' PROGRESS")
        check("b-base-backup-0A000", codes == ["0A000"], f"{codes}")
        r.close()
        check("b-server-alive", alive(srv.port))
    finally:
        srv.stop()

# ---------------------------------------------------------------------------
# C. slot lifecycle
# ---------------------------------------------------------------------------

def t_slot_lifecycle():
    srv = Server()
    try:
        r = ReplConn(srv.port)
        tags, rows, codes, descs = r.repl_query(
            "CREATE_REPLICATION_SLOT myslot LOGICAL rustgres_decoding")
        check("c-create-shape",
              not codes and descs == [("slot_name", "consistent_point",
                                       "snapshot_name", "output_plugin")],
              f"{codes} {descs}")
        check("c-create-tag", tags == ["CREATE_REPLICATION_SLOT"], f"{tags}")
        name, cp, snap, plugin = rows[0]
        check("c-create-row", name == "myslot" and snap is None
              and plugin == "rustgres_decoding", f"{rows[0]}")
        parse_lsn(cp)  # raises if malformed
        check("c-create-cp-valid", True)
        _, _, codes, _ = r.repl_query(
            "CREATE_REPLICATION_SLOT myslot LOGICAL rustgres_decoding")
        check("c-duplicate-42710", codes == ["42710"], f"{codes}")
        _, _, codes, _ = r.repl_query(
            "CREATE_REPLICATION_SLOT BadName LOGICAL rustgres_decoding")
        check("c-badname-42602", codes == ["42602"], f"{codes}")
        _, _, codes, _ = r.repl_query(
            "CREATE_REPLICATION_SLOT s2 LOGICAL test_decoding")
        check("c-badplugin-0A000", codes == ["0A000"], f"{codes}")
        # pg_replication_slots exact shape.
        c = Conn(srv.port)
        _, rows, codes, descs = c.q(
            "SELECT slot_name, plugin, slot_type, active, restart_lsn,"
            " confirmed_flush_lsn FROM pg_replication_slots")
        check("c-catalog-shape",
              not codes and descs == [("slot_name", "plugin", "slot_type",
                                       "active", "restart_lsn",
                                       "confirmed_flush_lsn")],
              f"{codes} {descs}")
        check("c-catalog-row",
              rows == [("myslot", "rustgres_decoding", "logical", "f", cp, cp)],
              f"{rows}")
        # DROP then drop-missing.
        _, _, codes, _ = r.repl_query("DROP_REPLICATION_SLOT myslot")
        check("c-drop-ok", codes == [], f"{codes}")
        _, rows, codes, _ = c.q("SELECT COUNT(*) FROM pg_replication_slots")
        check("c-drop-gone", not codes and rows == [("0",)], f"{codes} {rows}")
        _, _, codes, _ = r.repl_query("DROP_REPLICATION_SLOT myslot")
        check("c-drop-missing-42704", codes == ["42704"], f"{codes}")
        c.close(); r.close()
        check("c-server-alive", alive(srv.port))
    finally:
        srv.stop()

def t_slot_survives_crash():
    srv = Server()
    try:
        r = ReplConn(srv.port)
        _, rows, codes, _ = r.repl_query(
            "CREATE_REPLICATION_SLOT dur LOGICAL rustgres_decoding")
        assert not codes
        cp = rows[0][1]
        r.close()
        srv.kill9(); srv.restart()
        c = Conn(srv.port)
        _, rows, codes, _ = c.q(
            "SELECT slot_name, restart_lsn, confirmed_flush_lsn, active"
            " FROM pg_replication_slots")
        check("c-crash-row",
              not codes and rows == [("dur", cp, cp, "f")], f"{codes} {rows}")
        c.close()
        # Slot still usable after the crash.
        r2 = ReplConn(srv.port)
        _, _, codes, _ = r2.repl_query("DROP_REPLICATION_SLOT dur")
        check("c-crash-drop", codes == [], f"{codes}")
        r2.close()
    finally:
        srv.stop()

# ---------------------------------------------------------------------------
# D. logical streaming
# ---------------------------------------------------------------------------

def t_logical_streaming_exact():
    srv = Server()
    try:
        # Slot first: it only sees commits after its creation.
        r = ReplConn(srv.port)
        _, rows, codes, _ = r.repl_query(
            "CREATE_REPLICATION_SLOT stream1 LOGICAL rustgres_decoding")
        assert not codes
        cp = parse_lsn(rows[0][1])
        c = Conn(srv.port)
        c.q("CREATE TABLE t(id INT PRIMARY KEY, name TEXT)")
        c.q("INSERT INTO t VALUES (1, 'alice'), (2, 'bob')")
        c.q("UPDATE t SET name='bobby' WHERE id=2")
        c.q("DELETE FROM t WHERE id=1")
        c.q("CREATE TABLE u2(x INT)")
        st = r.start_replication("stream1", "0/0")
        lines = st.wait_for_lines(12, timeout=15)
        # Find the DDL + DML blocks; BEGIN/COMMIT LSNs must match within a
        # block and every data LSN must be >= the slot's consistent point.
        begins = [l for l in lines if l.startswith("BEGIN ")]
        commits = [l for l in lines if l.startswith("COMMIT ")]
        check("d-begin-commit-paired", len(begins) == len(commits) and len(begins) >= 5,
              f"{lines}")
        pairs_ok = all(b.split(" ")[1] == cm.split(" ")[1]
                       for b, cm in zip(begins, commits))
        check("d-begin-commit-lsn-match", pairs_ok, f"{lines}")
        check("d-lsns-after-cp",
              all(parse_lsn(l.split(" ")[1]) >= cp for l in begins),
              f"{lines}")
        body = [l for l in lines
                if not l.startswith("BEGIN ") and not l.startswith("COMMIT ")]
        check("d-insert-exact",
              "INSERT t id=2 name='bob'" in body and
              "INSERT t id=1 name='alice'" in body, f"{body}")
        check("d-update-exact",
              "UPDATE t OLD id=2 name='bob' NEW id=2 name='bobby'" in body,
              f"{body}")
        check("d-delete-exact",
              "DELETE t id=1 name='alice'" in body, f"{body}")
        check("d-ddl-markers",
              "DDL CREATE_TABLE t" in body and "DDL CREATE_TABLE u2" in body,
              f"{body}")
        # Keepalive arrives while idle (<= KEEPALIVE_INTERVAL + slack).
        _, keep = st.poll(timeout=14)
        check("d-keepalive", len(keep) >= 1, f"keepalives={keep}")
        if keep:
            check("d-keepalive-noreply", all(k[1] == 0 for k in keep), f"{keep}")
        # Standby status advances the confirmed flush position (visible
        # immediately via pg_replication_slots). Use the last XLogData
        # wal_end we saw.
        xlog, _ = st.poll(timeout=1)
        last_wal_end = max([w for _, w, _ in xlog] + [cp])
        st.standby_status(last_wal_end)
        time.sleep(0.5)
        _, rows, codes, _ = c.q(
            "SELECT confirmed_flush_lsn FROM pg_replication_slots"
            " WHERE slot_name='stream1'")
        expect = "%X/%X" % (last_wal_end >> 32, last_wal_end & 0xFFFFFFFF)
        check("d-flush-tracked",
              not codes and rows == [(expect,)], f"{codes} {rows} want {expect}")
        # Active flag set during streaming...
        _, rows, codes, _ = c.q(
            "SELECT active FROM pg_replication_slots WHERE slot_name='stream1'")
        check("d-active-true", not codes and rows == [("t",)], f"{codes} {rows}")
        st.copy_done()
        time.sleep(0.3)
        _, rows, codes, _ = c.q(
            "SELECT active FROM pg_replication_slots WHERE slot_name='stream1'")
        check("d-active-false", not codes and rows == [("f",)], f"{codes} {rows}")
        # ... and the connection is still usable for more commands.
        _, _, codes, _ = r.repl_query("DROP_REPLICATION_SLOT stream1")
        check("d-drop-after-stream", codes == [], f"{codes}")
        c.close(); r.close()
        check("d-server-alive", alive(srv.port))
    finally:
        srv.stop()

def t_start_replication_errors():
    srv = Server()
    try:
        r = ReplConn(srv.port)

        def repl_err(sql):
            """Send a raw replication command; drain to ReadyForQuery and
            return the ErrorResponse code (None if no error)."""
            r.s.sendall(msg(b"Q", cstr(sql)))
            code = None
            while True:
                t, p = r._read_msg(timeout=10)
                if t == b"E":
                    code = err_code(p)
                elif t == b"Z":
                    break
            return code

        # Missing slot -> 42704.
        check("e-missing-slot",
              repl_err("START_REPLICATION SLOT nosuch LOGICAL 0/0") == "42704",
              "no 42704")
        # Physical slot -> 0A000 on START.
        _, _, codes, _ = r.repl_query("CREATE_REPLICATION_SLOT phys PHYSICAL")
        check("e-phys-create-ok", codes == [], f"{codes}")
        check("e-phys-start-0A000",
              repl_err("START_REPLICATION SLOT phys LOGICAL 0/0") == "0A000",
              "no 0A000")
        # Bad LSN -> 42602.
        _, _, codes, _ = r.repl_query(
            "CREATE_REPLICATION_SLOT s3 LOGICAL rustgres_decoding")
        assert not codes
        check("e-bad-lsn-42602",
              repl_err("START_REPLICATION SLOT s3 LOGICAL zz") == "42602",
              "no 42602")
        # Drop while active -> 55006.
        st = r.start_replication("s3", "0/0")
        r2 = ReplConn(srv.port)
        _, _, codes, _ = r2.repl_query("DROP_REPLICATION_SLOT s3")
        check("e-drop-active-55006", codes == ["55006"], f"{codes}")
        st.copy_done()
        r2.close(); r.close()
        check("e-server-alive", alive(srv.port))
    finally:
        srv.stop()

TESTS = [
    t_regular_clients_unaffected,
    t_identify_system,
    t_replication_requires_superuser,
    t_replication_command_errors,
    t_slot_lifecycle,
    t_slot_survives_crash,
    t_logical_streaming_exact,
    t_start_replication_errors,
]

if __name__ == "__main__":
    if not os.path.exists(BIN):
        print(f"missing {BIN}; build first")
        sys.exit(2)
    for t in TESTS:
        print(f"== {t.__name__}")
        try:
            t()
        except Exception as e:
            failed.append(t.__name__)
            print(f"  FAIL: {t.__name__} raised {e!r}")
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    if failed:
        print("failures:", failed)
        sys.exit(1)
