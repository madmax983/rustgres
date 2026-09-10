#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.4 (WAL durability, checkpoints, crash
recovery).

Unlike the v0.1-v0.3 suites this one manages its own server subprocesses:
it starts the server with a fresh RUSTGRES_DATA_DIR per test, SIGKILLs it
(`kill -9`) at chosen moments, restarts it against the same data dir, and
verifies over the wire protocol that committed data survived and
uncommitted data did not.

Usage: `cargo build` first, then `python3 tests/protocol_test4.py`.
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


# ---------------------------------------------------------------------------
# Wire framing (mirrors tests/protocol_test.py)
# ---------------------------------------------------------------------------

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
                        pos += ln
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


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

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
    """A rustgres subprocess bound to a fresh data dir."""

    def __init__(self):
        self.data_dir = tempfile.mkdtemp(prefix="rg4_")
        self.proc = None

    def start(self):
        env = dict(os.environ, RUSTGRES_DATA_DIR=self.data_dir)
        self.proc = subprocess.Popen(
            [BIN], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if not wait_for_port():
            raise RuntimeError("server did not open 127.0.0.1:5433")
        # give it a beat to finish recovery printing before we connect
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

    def wal_size(self):
        p = os.path.join(self.data_dir, "wal.log")
        return os.path.getsize(p) if os.path.exists(p) else -1

    def has_checkpoint(self):
        return os.path.exists(os.path.join(self.data_dir, "checkpoint.dat"))

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

def t_committed_survives_kill9():
    print("== committed data survives kill -9 ==")
    srv = fresh_server()
    try:
        c = Conn()
        check("create", c.q("CREATE TABLE t(a INT, b TEXT)")[0] == ["CREATE TABLE"])
        check("insert", c.q("INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three')")[0] == ["INSERT 0 3"])
        check("wal non-empty after commits", srv.wal_size() > 0, f"size={srv.wal_size()}")
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        # (no ORDER BY in the v0.4 SQL subset; insertion order is stable)
        tags, rows, _ = c.q("SELECT * FROM t")
        check("all committed rows present after kill -9",
              rows == [["1", "one"], ["2", "two"], ["3", "three"]], f"{rows}")
        c.close()
    finally:
        srv.cleanup()


def t_uncommitted_lost():
    print("== uncommitted transaction does not survive kill -9 ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("INSERT INTO t VALUES (1)")
        check("begin", c.q("BEGIN")[0] == ["BEGIN"])
        check("uncommitted insert", c.q("INSERT INTO t VALUES (2), (3)")[0] == ["INSERT 0 2"])
        # kill -9 with the transaction still open (no COMMIT)
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        check("uncommitted rows absent after kill -9", rows == [["1"]], f"{rows}")
        c.close()
    finally:
        srv.cleanup()


def t_uncommitted_ddl_lost():
    print("== uncommitted DDL does not survive kill -9 ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE keepme(a INT)")
        check("begin", c.q("BEGIN")[0] == ["BEGIN"])
        check("create in txn", c.q("CREATE TABLE doomed(a INT)")[0] == ["CREATE TABLE"])
        check("insert in txn", c.q("INSERT INTO doomed VALUES (9)")[0] == ["INSERT 0 1"])
        srv.kill9()
        srv.start()
        c = Conn()
        _, _, codes = c.q("SELECT * FROM doomed")
        check("uncommitted table absent after kill -9", codes == ["42P01"], f"{codes}")
        _, rows, _ = c.q("SELECT * FROM keepme")
        check("committed table still there", rows == [], f"{rows}")
        c.close()
    finally:
        srv.cleanup()


def t_ddl_durable():
    print("== committed DDL (create/drop) survives kill -9 ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE a(x INT)")
        c.q("INSERT INTO a VALUES (7)")
        c.q("CREATE TABLE b(y TEXT)")
        c.q("DROP TABLE b")
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM a")
        check("created+inserted table survived", rows == [["7"]], f"{rows}")
        _, _, codes = c.q("SELECT * FROM b")
        check("dropped table stayed dropped", codes == ["42P01"], f"{codes}")
        c.close()
    finally:
        srv.cleanup()


def t_many_acked_commits_durable():
    print("== every acked commit is durable (kill -9 right after last ack) ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        n = 200
        for i in range(n):
            tags, _, _ = c.q(f"INSERT INTO t VALUES ({i})")
            assert tags == ["INSERT 0 1"], f"insert {i} failed: {tags}"
        c.close()
        srv.kill9()  # immediately after the 200th COMMIT ack
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        got = sorted(int(r[0]) for r in rows)
        check(f"all {n} acked rows present", got == list(range(n)),
              f"got {len(rows)} rows")
        c.close()
    finally:
        srv.cleanup()


def t_checkpoint_truncates_wal():
    print("== CHECKPOINT snapshots and truncates the WAL ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT, b TEXT)")
        for i in range(50):
            c.q(f"INSERT INTO t VALUES ({i}, 'v{i}')")
        before = srv.wal_size()
        check("wal grew before checkpoint", before > 0, f"size={before}")
        check("checkpoint tag", c.q("CHECKPOINT")[0] == ["CHECKPOINT"])
        # Post-checkpoint wal.log holds just the 16-byte generation header.
        check("wal reset to fresh generation after checkpoint",
              srv.wal_size() == 16, f"size={srv.wal_size()}")
        check("checkpoint file exists", srv.has_checkpoint())
        _, rows, _ = c.q("SELECT * FROM t")
        check("data intact after checkpoint", len(rows) == 50, f"{len(rows)}")
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        check("data intact after kill -9 post-checkpoint",
              len(rows) == 50 and rows[0] == ["0", "v0"] and rows[-1] == ["49", "v49"],
              f"{len(rows)} rows")
        c.close()
    finally:
        srv.cleanup()


def t_checkpoint_then_writes():
    print("== writes after a checkpoint replay correctly ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("INSERT INTO t VALUES (1)")
        c.q("CHECKPOINT")
        c.q("INSERT INTO t VALUES (2)")
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (3)")
        c.q("COMMIT")
        c.q("CHECKPOINT")  # second checkpoint must also work
        c.q("INSERT INTO t VALUES (4)")
        check("wal has post-checkpoint frames", srv.wal_size() > 0)
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        check("checkpoint + replayed tail all present",
              [r[0] for r in rows] == ["1", "2", "3", "4"], f"{rows}")
        c.close()
    finally:
        srv.cleanup()


def t_long_wal_recovery():
    print("== long WAL (no checkpoint) replays fully ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT, b TEXT, d BOOL)")
        n = 1500
        for i in range(n):
            tags, _, _ = c.q(f"INSERT INTO t VALUES ({i}, 'name{i}', true)")
            assert tags == ["INSERT 0 1"], f"insert {i} failed"
        check("wal is long", srv.wal_size() > 20000, f"size={srv.wal_size()}")
        check("no checkpoint yet", not srv.has_checkpoint())
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        ok = (len(rows) == n and rows[0] == ["0", "name0", "t"]
              and rows[-1] == [str(n - 1), f"name{n-1}", "t"])
        check(f"all {n} rows replayed from WAL", ok, f"got {len(rows)} rows")
        c.close()
    finally:
        srv.cleanup()


def t_rollback_not_durable():
    print("== rolled-back data is absent after kill -9 ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("BEGIN")
        c.q("INSERT INTO t VALUES (1)")
        check("rollback", c.q("ROLLBACK")[0] == ["ROLLBACK"])
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        check("rolled-back row absent", rows == [], f"{rows}")
        c.close()
    finally:
        srv.cleanup()


def t_checkpoint_in_txn_errors():
    print("== CHECKPOINT inside a transaction is rejected ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("BEGIN")
        _, _, codes = c.q("CHECKPOINT")
        check("CHECKPOINT in txn -> 25001", codes == ["25001"], f"{codes}")
        check("rollback after", c.q("ROLLBACK")[0] == ["ROLLBACK"])
        c.close()
    finally:
        srv.cleanup()


def t_data_dir_created():
    print("== data dir is created when missing ==")
    base = tempfile.mkdtemp(prefix="rg4_parent_")
    nested = os.path.join(base, "no", "such", "dir", "yet")
    assert not os.path.exists(nested)
    env = dict(os.environ, RUSTGRES_DATA_DIR=nested)
    proc = subprocess.Popen([BIN], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not wait_for_port():
            raise RuntimeError("server did not start")
        time.sleep(0.2)
        check("nested data dir created", os.path.isdir(nested))
        check("wal.log created", os.path.exists(os.path.join(nested, "wal.log")))
        c = Conn()
        check("server works in fresh dir",
              c.q("CREATE TABLE t(a INT)")[0] == ["CREATE TABLE"])
        c.close()
    finally:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)
        shutil.rmtree(base, ignore_errors=True)


def t_txn_commit_durable():
    print("== explicit-transaction COMMIT is durable across kill -9 ==")
    srv = fresh_server()
    try:
        c = Conn()
        c.q("CREATE TABLE t(a INT)")
        c.q("BEGIN")
        for i in range(10):
            c.q(f"INSERT INTO t VALUES ({i})")
        check("commit", c.q("COMMIT")[0] == ["COMMIT"])
        c.close()
        srv.kill9()
        srv.start()
        c = Conn()
        _, rows, _ = c.q("SELECT * FROM t")
        check("txn-committed rows survived",
              [r[0] for r in rows] == [str(i) for i in range(10)], f"{rows}")
        c.close()
    finally:
        srv.cleanup()


def main():
    if not os.path.exists(BIN):
        print(f"build the server first: cargo build (missing {BIN})")
        return 2
    # Refuse to run against a stranger's server. SO_REUSEADDR here so
    # lingering TIME_WAIT ghosts don't read as "in use" — a live listener
    # still refuses the bind either way.
    probe = socket.socket()
    probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        probe.bind((HOST, PORT))
    except OSError:
        print(f"ERROR: {HOST}:{PORT} is already in use — stop the other server first.")
        return 1
    finally:
        probe.close()

    tests = [
        t_data_dir_created,
        t_committed_survives_kill9,
        t_uncommitted_lost,
        t_uncommitted_ddl_lost,
        t_ddl_durable,
        t_txn_commit_durable,
        t_rollback_not_durable,
        t_many_acked_commits_durable,
        t_checkpoint_truncates_wal,
        t_checkpoint_then_writes,
        t_long_wal_recovery,
        t_checkpoint_in_txn_errors,
    ]
    for t in tests:
        try:
            t()
        except Exception as e:
            failed.append(t.__name__)
            print(f"  FAIL: {t.__name__} raised {type(e).__name__}: {e}")

    print(f"\n{len(passed)} passed, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
