#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.5 (MVCC, isolation levels, VACUUM).

Covers: uncommitted/aborted invisibility, own-writes-visible, READ
COMMITTED fresh snapshots, REPEATABLE READ stability, RR write-conflict
40001, SERIALIZABLE as snapshot isolation (write skew is NOT prevented),
savepoint rollback of MVCC writes, concurrent UPDATE races (40001 on
lost update), VACUUM / VACUUM VERBOSE reclamation with snapshot pinning,
UPDATE/DELETE basics, isolation-level syntax, VACUUM/CHECKPOINT outside
transactions only, extended-protocol MVCC, and crash recovery of
UPDATE/DELETE chains.

Usage: `cargo build` first, then `python3 tests/protocol_test5.py`.
Requires a free 127.0.0.1:5433.
"""
import os
import shutil
import signal
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

passed = []
failed = []


def check(name, cond, detail=""):
    if cond:
        passed.append(name)
        print(f"  ok: {name}")
    else:
        failed.append(name)
        print(f"  FAIL: {name} {detail}")


def msg(typ, body):
    return typ + struct.pack("!i", len(body) + 4) + body


def cstr(s):
    return s.encode() + b"\x00"


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self._drain_until_ready()

    def _read_msg(self):
        t = self.s.recv(1)
        if not t:
            raise RuntimeError("connection closed by server")
        (ln,) = struct.unpack("!i", self._read_exact(4))
        return t, self._read_exact(ln - 4)

    def _read_exact(self, n):
        data = b""
        while len(data) < n:
            chunk = self.s.recv(n - len(data))
            if not chunk:
                raise RuntimeError("connection closed by server")
            data += chunk
        return data

    def _drain_until_ready(self):
        while True:
            t, _ = self._read_msg()
            if t == b"Z":
                return

    def q(self, sql):
        """Returns (tags, rows, err_codes)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        tags, rows, codes = [], [], []
        while True:
            t, p = self._read_msg()
            if t == b"C":
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
                    pos += ln if ln != -1 else 0
                rows.append(r)
            elif t == b"E":
                fields = {}
                pos = 0
                while p[pos] != 0:
                    ftype = chr(p[pos])
                    end = p.index(b"\x00", pos + 1)
                    fields[ftype] = p[pos + 1:end].decode()
                    pos = end + 1
                codes.append(fields.get("C", "?"))
            elif t == b"Z":
                return tags, rows, codes

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        finally:
            self.s.close()

    def close_abrupt(self):
        # Raw socket close with no Terminate message: the server must notice
        # EOF on its next read and roll the open transaction back, exactly
        # like a crashed client.
        self.s.close()


def wait_for_port(timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            s = socket.create_connection((HOST, PORT), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.1)
    return False


class Server:
    def __init__(self):
        self.data_dir = tempfile.mkdtemp(prefix="rg5_")
        self.proc = None

    def start(self):
        env = dict(os.environ, RUSTGRES_DATA_DIR=self.data_dir)
        self.proc = subprocess.Popen(
            [BIN], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if not wait_for_port():
            raise RuntimeError("server did not open 127.0.0.1:5433")
        time.sleep(0.2)

    def kill9(self):
        assert self.proc is not None
        self.proc.send_signal(signal.SIGKILL)
        self.proc.wait(timeout=10)
        self.proc = None
        time.sleep(0.3)

    def stop(self):
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
            self.proc = None

    def cleanup(self):
        self.stop()
        shutil.rmtree(self.data_dir, ignore_errors=True)


def fresh_server():
    srv = Server()
    srv.start()
    return srv


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def t_uncommitted_invisible():
    print("== uncommitted writes invisible to others ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("BEGIN")
        check("insert uncommitted", c1.q("INSERT INTO t VALUES (1)")[0] == ["INSERT 0 1"])
        check("other session sees nothing", c2.q("SELECT * FROM t")[1] == [])
        check("own session sees own write", c1.q("SELECT * FROM t")[1] == [["1"]])
        c1.q("COMMIT")
        check("visible after commit", c2.q("SELECT * FROM t")[1] == [["1"]])
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_aborted_invisible():
    print("== aborted writes invisible; failed txn is 25P02 ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("BEGIN")
        c1.q("INSERT INTO t VALUES (1)")
        c1.q("ROLLBACK")
        check("rolled back invisible", c2.q("SELECT * FROM t")[1] == [])
        # failed transaction: error then 25P02 until ROLLBACK
        c1.q("BEGIN")
        c1.q("INSERT INTO t VALUES (2)")
        tags, rows, codes = c1.q("INSERT INTO missing VALUES (1)")
        check("bad statement errors", codes == ["42P01"], str(codes))
        tags, rows, codes = c1.q("SELECT * FROM t")
        check("25P02 in aborted txn", codes == ["25P02"], str(codes))
        c1.q("ROLLBACK")
        check("failed txn's writes gone", c2.q("SELECT * FROM t")[1] == [])
        check("txn usable again", c1.q("INSERT INTO t VALUES (3)")[0] == ["INSERT 0 1"])
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_own_writes_visible():
    print("== own writes visible inside the transaction ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT, b TEXT)")
        c.q("INSERT INTO t VALUES (1, 'one')")
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (2, 'two')")
        c.q("UPDATE t SET b = 'TWO' WHERE a = 2")
        c.q("DELETE FROM t WHERE a = 1")
        check("sees own insert+update, not own delete",
              c.q("SELECT * FROM t ORDER BY a")[1] == [["2", "TWO"]])
        c.q("COMMIT")
        check("committed state matches",
              c.q("SELECT * FROM t ORDER BY a")[1] == [["2", "TWO"]])
        c.close()
    finally:
        srv.cleanup()


def t_read_committed():
    print("== READ COMMITTED: fresh snapshot per statement ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("INSERT INTO t VALUES (1)")
        c1.q("BEGIN")  # default = READ COMMITTED
        check("sees 1 row", len(c1.q("SELECT a FROM t")[1]) == 1)
        c2.q("INSERT INTO t VALUES (2)")
        check("sees newly committed row (no phantom protection)",
              len(c1.q("SELECT a FROM t")[1]) == 2)
        c2.q("UPDATE t SET a = 20 WHERE a = 2")
        check("sees committed update",
              c1.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["20"]])
        c1.q("COMMIT")
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_repeatable_read():
    print("== REPEATABLE READ: stable snapshot, no phantoms ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("INSERT INTO t VALUES (1)")
        c1.q("BEGIN ISOLATION LEVEL REPEATABLE READ")
        check("initial read", len(c1.q("SELECT a FROM t")[1]) == 1)
        c2.q("INSERT INTO t VALUES (2)")
        c2.q("UPDATE t SET a = 10 WHERE a = 1")
        check("no phantom", len(c1.q("SELECT a FROM t")[1]) == 1)
        check("no committed update visible",
              c1.q("SELECT a FROM t")[1] == [["1"]])
        c1.q("COMMIT")
        check("after commit sees everything",
              c1.q("SELECT a FROM t ORDER BY a")[1] == [["2"], ["10"]])
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_rr_write_conflict():
    print("== REPEATABLE READ: 40001 on concurrent update ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("INSERT INTO t VALUES (1)")
        c1.q("BEGIN ISOLATION LEVEL REPEATABLE READ")
        c1.q("SELECT * FROM t")  # pin the snapshot
        c2.q("UPDATE t SET a = 2 WHERE a = 1")
        tags, rows, codes = c1.q("UPDATE t SET a = 3 WHERE a = 1")
        check("40001 on stale write", codes == ["40001"], str((tags, codes)))
        # transaction is now aborted
        tags, rows, codes = c1.q("SELECT * FROM t")
        check("25P02 after 40001", codes == ["25P02"], str(codes))
        c1.q("ROLLBACK")
        check("loser's update gone, winner's stands",
              c2.q("SELECT a FROM t")[1] == [["2"]])
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_serializable_is_snapshot_isolation():
    print("== SERIALIZABLE = snapshot isolation (write skew allowed) ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE acct(name TEXT, bal INT)")
        c1.q("INSERT INTO acct VALUES ('a', 100), ('b', 100)")
        c1.q("BEGIN ISOLATION LEVEL SERIALIZABLE")
        c2.q("BEGIN ISOLATION LEVEL SERIALIZABLE")
        def total(c):
            return sum(int(r[0]) for r in c.q("SELECT bal FROM acct")[1])
        check("c1 sees 200", total(c1) == 200)
        check("c2 sees 200", total(c2) == 200)
        # Write skew: each zeroes a different account after seeing the
        # other is funded. True serializable (SSI) would abort one.
        c1.q("UPDATE acct SET bal = 0 WHERE name = 'a'")
        c2.q("UPDATE acct SET bal = 0 WHERE name = 'b'")
        check("c1 commits", c1.q("COMMIT")[0] == ["COMMIT"])
        check("c2 commits (write skew NOT prevented)",
              c2.q("COMMIT")[0] == ["COMMIT"])
        check("both zeroed",
              c1.q("SELECT bal FROM acct ORDER BY name")[1] == [["0"], ["0"]])
        # ...but the snapshot itself is stable like REPEATABLE READ.
        c1.q("BEGIN ISOLATION LEVEL SERIALIZABLE")
        c1.q("SELECT * FROM acct")
        c2.q("UPDATE acct SET bal = 50 WHERE name = 'a'")
        check("serializable snapshot stable",
              c1.q("SELECT bal FROM acct WHERE name = 'a'")[1] == [["0"]])
        c1.q("COMMIT")
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_savepoint_rollback():
    print("== SAVEPOINT rolls back MVCC writes ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (1)")
        c.q("SAVEPOINT sp1")
        c.q("INSERT INTO t VALUES (2)")
        c.q("UPDATE t SET a = 20 WHERE a = 2")
        c.q("ROLLBACK TO SAVEPOINT sp1")
        check("only pre-savepoint row remains",
              c.q("SELECT a FROM t ORDER BY a")[1] == [["1"]])
        c.q("COMMIT")
        check("durable", c.q("SELECT a FROM t ORDER BY a")[1] == [["1"]])
        # savepoint also recovers from an aborted txn
        c.q("BEGIN")
        c.q("SAVEPOINT sp2")
        c.q("INSERT INTO missing VALUES (1)")
        c.q("ROLLBACK TO SAVEPOINT sp2")
        check("usable after rollback-to", c.q("INSERT INTO t VALUES (5)")[0] == ["INSERT 0 1"])
        c.q("COMMIT")
        check("row 5 durable", c.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["5"]])
        c.close()
    finally:
        srv.cleanup()


def t_update_delete_basics():
    print("== UPDATE/DELETE basics + tags ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT, b TEXT)")
        c.q("INSERT INTO t VALUES (1,'a'), (2,'b'), (3,'c')")
        check("update tag", c.q("UPDATE t SET b='z' WHERE a=2")[0] == ["UPDATE 1"])
        check("update effect",
              c.q("SELECT b FROM t WHERE a=2")[1] == [["z"]])
        check("delete tag", c.q("DELETE FROM t WHERE a=3")[0] == ["DELETE 1"])
        check("delete effect",
              c.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["2"]])
        check("update all", c.q("UPDATE t SET a=a+10")[0] == ["UPDATE 2"])
        check("values", c.q("SELECT a FROM t ORDER BY a")[1] == [["11"], ["12"]])
        c.close()
    finally:
        srv.cleanup()


def t_concurrent_update_race():
    print("== concurrent uncommitted UPDATEs: loser gets 40001 ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("INSERT INTO t VALUES (1)")
        c1.q("BEGIN")
        c2.q("BEGIN")
        c1.q("UPDATE t SET a = 10 WHERE a = 1")  # uncommitted
        c2.q("UPDATE t SET a = 20 WHERE a = 1")  # uncommitted, no block
        tags, rows, codes = c1.q("COMMIT")
        check("first committer gets 40001 (lost update)",
              codes == ["40001"], str((tags, codes)))
        c1.q("ROLLBACK")  # clean up the failed txn
        check("second committer wins", c2.q("COMMIT")[0] == ["COMMIT"])
        check("final value 20", c1.q("SELECT a FROM t")[1] == [["20"]])
        check("exactly one row", len(c1.q("SELECT a FROM t")[1]) == 1)
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_disconnect_rollback():
    print("== disconnect rolls back the open transaction ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("INSERT INTO t VALUES (1)")
        # Abrupt disconnect (no Terminate) with an uncommitted INSERT.
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (2)")
        c.close_abrupt()
        time.sleep(0.5)  # let the server thread observe EOF
        b = Conn()
        check("uncommitted insert invisible after abrupt disconnect",
              b.q("SELECT a FROM t ORDER BY a")[1] == [["1"]])
        check("aborted insert is deletable (no ghost row)",
              b.q("DELETE FROM t")[0] == ["DELETE 1"])
        # Clean Terminate with an uncommitted DELETE.
        b.q("INSERT INTO t VALUES (10)")
        b.q("BEGIN")
        b.q("DELETE FROM t")
        b.close()  # sends Terminate
        time.sleep(0.5)
        d = Conn()
        check("uncommitted delete rolled back on Terminate",
              d.q("SELECT a FROM t")[1] == [["10"]])
        # Abrupt disconnect with uncommitted DDL.
        d.q("BEGIN")
        d.q("CREATE TABLE ghost(x INT)")
        d.q("INSERT INTO ghost VALUES (1)")
        d.close_abrupt()
        time.sleep(0.5)
        e = Conn()
        tags, rows, codes = e.q("SELECT x FROM ghost")
        check("uncommitted DDL invisible after abrupt disconnect",
              codes == ["42P01"], str(codes))
        check("server still healthy after disconnects",
              e.q("SELECT a FROM t")[1] == [["10"]])
        e.q("VACUUM")
        check("vacuum runs clean after disconnects",
              e.q("VACUUM VERBOSE")[1] == [["vacuum: nothing to remove"]])
        e.close()
    finally:
        srv.cleanup()


def t_vacuum():
    print("== VACUUM reclaims; open snapshots pin versions ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        for i in range(0, 200, 20):
            vals = ", ".join(f"({j})" for j in range(i, i + 20))
            c1.q(f"INSERT INTO t VALUES {vals}")
        check("200 rows", len(c1.q("SELECT a FROM t")[1]) == 200)
        # Pin an old snapshot in another session.
        c2.q("BEGIN ISOLATION LEVEL REPEATABLE READ")
        c2.q("SELECT a FROM t")
        c1.q("DELETE FROM t")
        tags, rows, codes = c1.q("VACUUM VERBOSE t")
        check("vacuum verbose reports 0 while pinned",
              rows == [["table \"t\": removed 0 dead row version(s)"]], str(rows))
        c2.q("COMMIT")  # release the pin
        tags, rows, codes = c1.q("VACUUM VERBOSE t")
        check("vacuum verbose reports 200 after pin released",
              rows == [["table \"t\": removed 200 dead row version(s)"]], str(rows))
        check("plain vacuum tag", c1.q("VACUUM")[0] == ["VACUUM"])
        check("verbose, nothing left",
              c1.q("VACUUM VERBOSE")[1] == [["vacuum: nothing to remove"]])
        tags, rows, codes = c1.q("VACUUM missing")
        check("vacuum missing table is 42P01", codes == ["42P01"], str(codes))
        c1.close(); c2.close()
    finally:
        srv.cleanup()


def t_vacuum_checkpoint_in_txn():
    print("== VACUUM/CHECKPOINT refused inside a transaction ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("BEGIN")
        tags, rows, codes = c.q("VACUUM")
        check("VACUUM in txn is 25001", codes == ["25001"], str(codes))
        tags, rows, codes = c.q("CHECKPOINT")
        check("CHECKPOINT in txn is 25001", codes == ["25001"], str(codes))
        c.q("ROLLBACK")
        check("VACUUM outside txn works", c.q("VACUUM")[0] == ["VACUUM"])
        check("CHECKPOINT outside txn works", c.q("CHECKPOINT")[0] == ["CHECKPOINT"])
        c.close()
    finally:
        srv.cleanup()


def t_isolation_syntax():
    print("== isolation level syntax ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        for lvl in ["READ COMMITTED", "REPEATABLE READ", "SERIALIZABLE",
                    "READ UNCOMMITTED"]:
            tags, rows, codes = c.q(f"BEGIN ISOLATION LEVEL {lvl}")
            check(f"begin {lvl}", tags == ["BEGIN"] and not codes, str((tags, codes)))
            c.q("ROLLBACK")
        tags, rows, codes = c.q("BEGIN ISOLATION LEVEL CHAOS")
        check("bad level is syntax error", codes == ["42601"], str(codes))
        c.close()
    finally:
        srv.cleanup()


def t_order_by():
    print("== ORDER BY: sorting, direction, nulls, positional ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT, b TEXT)")
        c.q("INSERT INTO t VALUES (3, 'c'), (1, 'a'), (2, 'b')")
        check("asc", c.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["2"], ["3"]])
        check("desc", c.q("SELECT a FROM t ORDER BY a DESC")[1] == [["3"], ["2"], ["1"]])
        check("explicit asc", c.q("SELECT a FROM t ORDER BY a ASC")[1] == [["1"], ["2"], ["3"]])
        check("sort by non-selected column",
              c.q("SELECT b FROM t ORDER BY a")[1] == [["a"], ["b"], ["c"]])
        check("positional", c.q("SELECT a FROM t ORDER BY 1 DESC")[1] == [["3"], ["2"], ["1"]])
        check("multi-term",
              c.q("SELECT a FROM t ORDER BY b DESC, a")[1] == [["3"], ["2"], ["1"]])
        check("limit applies after sort",
              c.q("SELECT a FROM t ORDER BY a DESC LIMIT 2")[1] == [["3"], ["2"]])
        c.q("INSERT INTO t VALUES (NULL, 'n')")
        check("asc nulls last",
              c.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["2"], ["3"], [None]])
        check("desc nulls first",
              c.q("SELECT a FROM t ORDER BY a DESC")[1] == [[None], ["3"], ["2"], ["1"]])
        check("text sort", c.q("SELECT b FROM t ORDER BY b")[1] == [["a"], ["b"], ["c"], ["n"]])
        tags, rows, codes = c.q("SELECT a FROM t ORDER BY 9")
        check("bad position is 42601", codes == ["42601"], str(codes))
        tags, rows, codes = c.q("SELECT a FROM t ORDER BY nope")
        check("bad column is 42703", codes == ["42703"], str(codes))
        # ORDER BY sees MVCC snapshots too.
        c.q("BEGIN ISOLATION LEVEL REPEATABLE READ")
        c.q("SELECT a FROM t ORDER BY a")
        c2 = Conn()
        c2.q("INSERT INTO t VALUES (0, 'z')")
        check("rr snapshot stable under order by",
              c.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["2"], ["3"], [None]])
        c.q("COMMIT")
        check("new row sorts first",
              c.q("SELECT a FROM t ORDER BY a")[1][0] == ["0"])
        c.close(); c2.close()
    finally:
        srv.cleanup()


def t_extended_mvcc():
    print("== extended protocol: MVCC through Parse/Bind/Execute ==")
    srv = fresh_server()
    try:
        s = socket.create_connection((HOST, PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def read_msg():
            t = s.recv(1)
            (ln,) = struct.unpack("!i", read_exact(4))
            return t, read_exact(ln - 4)

        def read_exact(n):
            data = b""
            while len(data) < n:
                chunk = s.recv(n - len(data))
                if not chunk:
                    raise RuntimeError("closed")
                data += chunk
            return data

        def send(typ, body):
            s.sendall(typ + struct.pack("!i", len(body) + 4) + body)

        def drain_to_ready():
            while True:
                t, p = read_msg()
                if t == b"Z":
                    return p

        def simple(sql):
            send(b"Q", cstr(sql))
            tags, rows, codes = [], [], []
            while True:
                t, p = read_msg()
                if t == b"C":
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
                        pos += ln if ln != -1 else 0
                    rows.append(r)
                elif t == b"E":
                    codes.append("E")
                elif t == b"Z":
                    return tags, rows, codes, p

        drain_to_ready()
        simple("CREATE TABLE t(a INT)")
        simple("INSERT INTO t VALUES (1)")
        simple("BEGIN")
        # Prepared UPDATE with a parameter, executed inside the txn.
        send(b"P", cstr("upd") + cstr("UPDATE t SET a = $1 WHERE a = 1")
             + struct.pack("!h", 0))
        t, _ = read_msg()
        check("ParseComplete", t == b"1", str(t))
        send(b"B", cstr("") + cstr("upd") + struct.pack("!h", 0)
             + struct.pack("!h", 1) + struct.pack("!i", 2) + b"42"
             + struct.pack("!h", 0))
        t, _ = read_msg()
        check("BindComplete", t == b"2", str(t))
        send(b"E", cstr("") + struct.pack("!i", 0))
        t, p = read_msg()
        check("UPDATE 1 via portal", t == b"C" and p[:-1] == b"UPDATE 1",
              f"{t} {p}")
        send(b"S", b"")
        drain_to_ready()
        # Uncommitted: a second connection still sees the old value.
        c2 = Conn()
        check("uncommitted update invisible over extended protocol",
              c2.q("SELECT a FROM t")[1] == [["1"]])
        simple("COMMIT")
        check("committed update visible",
              c2.q("SELECT a FROM t")[1] == [["42"]])
        c2.close()
        send(b"X", b"")
        s.close()
    finally:
        srv.cleanup()


def t_crash_recovery_mvcc():
    print("== crash recovery: committed UPDATE/DELETE survive, uncommitted lost ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT, b TEXT)")
        c1.q("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        c1.q("BEGIN")
        c1.q("UPDATE t SET b = 'ONE' WHERE a = 1")  # uncommitted
        c2.q("DELETE FROM t WHERE a = 2")            # committed
        c1.q("INSERT INTO t VALUES (3, 'three')")    # uncommitted
        srv.kill9()
        srv.start()
        c = Conn()
        check("committed delete durable, uncommitted update+insert lost",
              c.q("SELECT a, b FROM t ORDER BY a")[1] == [["1", "one"]])
        # The engine still allocates fresh ids after recovery.
        c.q("INSERT INTO t VALUES (4, 'four')")
        check("post-recovery write works",
              c.q("SELECT a FROM t ORDER BY a")[1] == [["1"], ["4"]])
        c.close()
    finally:
        srv.cleanup()


def t_checkpoint_with_open_txn():
    print("== CHECKPOINT with concurrent open txn keeps recovery honest ==")
    srv = fresh_server()
    try:
        c1, c2 = Conn(), Conn()
        c1.q("CREATE TABLE t(a INT)")
        c1.q("INSERT INTO t VALUES (1)")
        c1.q("BEGIN")
        c1.q("INSERT INTO t VALUES (2)")  # uncommitted during checkpoint
        check("checkpoint with open txn", c2.q("CHECKPOINT")[0] == ["CHECKPOINT"])
        srv.kill9()
        srv.start()
        c = Conn()
        check("uncommitted row not resurrected",
              c.q("SELECT a FROM t ORDER BY a")[1] == [["1"]])
        c.close()
    finally:
        srv.cleanup()


def main():
    if not os.path.exists(BIN):
        print(f"build the server first: {BIN} missing")
        sys.exit(2)
    t_uncommitted_invisible()
    t_aborted_invisible()
    t_own_writes_visible()
    t_read_committed()
    t_repeatable_read()
    t_rr_write_conflict()
    t_serializable_is_snapshot_isolation()
    t_disconnect_rollback()
    t_vacuum()
    t_vacuum_checkpoint_in_txn()
    t_isolation_syntax()
    t_concurrent_update_race()
    t_extended_mvcc()
    t_crash_recovery_mvcc()
    t_checkpoint_with_open_txn()
    t_savepoint_rollback()
    t_update_delete_basics()
    t_order_by()
    print()
    print(f"{len(passed)} passed, {len(failed)} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
