#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.11 (SCRAM auth, roles, grants).

Covers SCRAM-SHA-256 authentication (correct / wrong password, unknown
user, tampered nonce, NOLOGIN, passwordless role, expired password),
trust-mode role resolution (unknown user, NOLOGIN), CREATE/ALTER/DROP
ROLE (incl. duplicate, missing, non-superuser, IF EXISTS, VALID UNTIL),
role GRANT/REVOKE membership (direct + transitive inheritance, dup,
self/cycle rejection, missing roles, non-superuser denial, DROP ROLE
cleanup, pg_auth_members), GRANT/REVOKE on tables (incl. column
privileges), sequences and the database, privilege enforcement on
SELECT/INSERT/UPDATE/DELETE/SELECT FOR UPDATE/nextval, ownership
enforcement on DDL (DROP/ALTER/OWNER TO/ANALYZE/VACUUM/CREATE OR
REPLACE VIEW), the pg_authid/pg_roles/pg_user/pg_auth_members catalogs
(incl. password masking and rolvaliduntil), CONNECTION LIMIT, and WAL
recovery of roles, memberships, grants, and column grants across a
restart (incl. CHECKPOINT images).

Each test boots a fresh server on a scratch data dir, so state can
never leak between tests. SCRAM tests boot in trust mode first to
create password roles, then reboot with RUSTGRES_AUTH=scram-sha-256.

Usage: build the server first (`cargo build`), then `python3 tests/protocol_test11.py`.
"""
import base64
import hashlib
import hmac
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

_next_port = [55843]


def alloc_port():
    _next_port[0] += 1
    return _next_port[0]


passed = []
failed = []


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
    code = ""
    i = 0
    while i < len(payload):
        if payload[i:i + 1] == b"C":
            j = payload.index(b"\x00", i + 1)
            code = payload[i + 1:j].decode()
            break
        j = payload.index(b"\x00", i + 1)
        i = j + 1
    return code


def _read_exact(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise RuntimeError("connection closed mid-message")
        data += chunk
    return data


class Conn:
    """Trust-mode connection as `user` (default postgres)."""

    def __init__(self, port, user="postgres", timeout=10):
        self.s = socket.create_connection((HOST, port), timeout=timeout)
        body = struct.pack("!i", 196608) + b"user\x00" + user.encode() + b"\x00\x00"
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
                raise RuntimeError("connection closed mid-message")
            data += chunk
        return data

    def _drain_until_ready(self):
        while True:
            t, p = self._read_msg()
            if t == b"Z":
                return
            if t == b"E":
                raise RuntimeError("auth failed: " + err_code(p))

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
                rows.append(tuple(r))
            elif t == b"E":
                codes.append(err_code(p))
            elif t == b"Z":
                break
        return tags, rows, codes

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        except OSError:
            pass
        self.s.close()


# ---------------------------------------------------------------------------
# SCRAM-SHA-256 client (RFC 7677)
# ---------------------------------------------------------------------------

def scram_exchange(sock_read, sock_write, user, password, tamper_nonce=False,
                   channel_binding="biws"):
    """Drive the client side of a SCRAM exchange. Returns (ok, server_error_code)."""
    client_nonce = base64.b64encode(os.urandom(18)).decode()
    client_first_bare = f"n={user},r={client_nonce}"
    # SASLInitialResponse
    sock_write(msg(b"p", cstr("SCRAM-SHA-256") + struct.pack("!i", len("n,," + client_first_bare))
                   + ("n,," + client_first_bare).encode()))
    t, p = sock_read()
    if t == b"E":
        return False, err_code(p)
    assert t == b"R" and struct.unpack("!i", p[:4])[0] == 11, (t, p[:4])
    server_first = p[4:].decode()
    attrs = dict(a.split("=", 1) for a in server_first.split(","))
    server_nonce, salt, iters = attrs["r"], base64.b64decode(attrs["s"]), int(attrs["i"])
    assert server_nonce.startswith(client_nonce)
    salted = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, iters, dklen=32)
    client_key = hmac.new(salted, b"Client Key", hashlib.sha256).digest()
    stored_key = hashlib.sha256(client_key).digest()
    if tamper_nonce:
        server_nonce = server_nonce[:-2] + ("xx" if not server_nonce.endswith("xx") else "yy")
    without_proof = f"c={channel_binding},r={server_nonce}"
    auth_msg = f"{client_first_bare},{server_first},{without_proof}".encode()
    sig = hmac.new(stored_key, auth_msg, hashlib.sha256).digest()
    proof = bytes(a ^ b for a, b in zip(client_key, sig))
    client_final = without_proof + ",p=" + base64.b64encode(proof).decode()
    sock_write(msg(b"p", client_final.encode()))
    t, p = sock_read()
    if t == b"E":
        return False, err_code(p)
    assert t == b"R" and struct.unpack("!i", p[:4])[0] == 12, (t, p[:4])
    server_final = p[4:].decode()
    # verify server signature
    server_key = hmac.new(salted, b"Server Key", hashlib.sha256).digest()
    expected = base64.b64encode(hmac.new(server_key, auth_msg, hashlib.sha256).digest()).decode()
    assert server_final == "v=" + expected, "server signature mismatch"
    # AuthenticationOk
    t, p = sock_read()
    if t == b"E":
        return False, err_code(p)
    assert t == b"R" and struct.unpack("!i", p[:4])[0] == 0, (t, p[:4])
    return True, ""


def scram_connect(port, user, password, **kw):
    """Returns (Conn-like socket wrapper, ok, err_code)."""
    s = socket.create_connection((HOST, port), timeout=10)
    body = struct.pack("!i", 196608) + b"user\x00" + user.encode() + b"\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)

    def read_msg():
        t = s.recv(1)
        if not t:
            raise RuntimeError("connection closed by server")
        (ln,) = struct.unpack("!i", read_exact(4))
        return t, read_exact(ln - 4)

    def read_exact(n):
        data = b""
        while len(data) < n:
            chunk = s.recv(n - len(data))
            if not chunk:
                raise RuntimeError("connection closed mid-message")
            data += chunk
        return data

    t, p = read_msg()
    assert t == b"R" and struct.unpack("!i", p[:4])[0] == 10, (t, p[:4])
    assert b"SCRAM-SHA-256\x00" in p
    ok, code = scram_exchange(read_msg, s.sendall, user, password, **kw)
    if not ok:
        s.close()
        return None, False, code
    # drain to ReadyForQuery
    while True:
        t, _ = read_msg()
        if t == b"Z":
            break
    return s, True, ""


# ---------------------------------------------------------------------------
# server harness
# ---------------------------------------------------------------------------

class Server:
    def __init__(self, auth=None):
        self.port = alloc_port()
        self.datadir = tempfile.mkdtemp(prefix="rg11-")
        env = dict(os.environ)
        if auth:
            env["RUSTGRES_AUTH"] = auth
        self.proc = subprocess.Popen(
            [BIN, "--port", str(self.port), "--data-dir", self.datadir],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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


def fresh_server(auth=None):
    return Server(auth=auth)


# ---------------------------------------------------------------------------
# tests
# ---------------------------------------------------------------------------

def t_role_lifecycle():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        tags, _, codes = c.q("CREATE ROLE alice LOGIN PASSWORD 's3cret'")
        check("create-role", tags == ["CREATE ROLE"] and not codes, str((tags, codes)))
        _, rows, _ = c.q("SELECT rolname, rolsuper, rolcanlogin, rolconnlimit FROM pg_roles WHERE rolname='alice'")
        check("pg-roles-row", rows == [("alice", "f", "t", "-1")], str(rows))
        _, rows, _ = c.q("SELECT rolpassword FROM pg_authid WHERE rolname='alice'")
        check("superuser-sees-verifier",
              len(rows) == 1 and rows[0][0] is not None and rows[0][0].startswith("SCRAM-SHA-256$"),
              str(rows)[:80])
        # duplicate
        _, _, codes = c.q("CREATE ROLE alice")
        check("create-role-dup", codes == ["42710"], str(codes))
        # USER alias
        tags, _, codes = c.q("CREATE USER bob")
        check("create-user-alias", tags == ["CREATE ROLE"] and not codes, str((tags, codes)))
        # ALTER ROLE
        tags, _, codes = c.q("ALTER ROLE alice NOLOGIN CONNECTION LIMIT 5")
        check("alter-role", tags == ["ALTER ROLE"] and not codes, str((tags, codes)))
        _, rows, _ = c.q("SELECT rolcanlogin, rolconnlimit FROM pg_roles WHERE rolname='alice'")
        check("alter-role-visible", rows == [("f", "5")], str(rows))
        # ALTER back
        c.q("ALTER ROLE alice LOGIN")
        # DROP ROLE
        tags, _, codes = c.q("DROP ROLE bob")
        check("drop-role", tags == ["DROP ROLE"] and not codes, str((tags, codes)))
        _, _, codes = c.q("DROP ROLE bob")
        check("drop-role-missing", codes == ["42704"], str(codes))
        tags, _, codes = c.q("DROP ROLE IF EXISTS bob")
        check("drop-role-if-exists", tags == ["DROP ROLE"] and not codes, str((tags, codes)))
        c.close()
    finally:
        srv.stop()


def t_role_perm_denied():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE TABLE t (a int)")
        c.close()
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("CREATE ROLE mallory")
        check("nosuper-create-role", codes == ["42501"], str(codes))
        _, _, codes = a.q("ALTER ROLE alice SUPERUSER")
        check("nosuper-alter-role", codes == ["42501"], str(codes))
        _, _, codes = a.q("DROP ROLE alice")
        check("nosuper-drop-role", codes == ["42501"], str(codes))
        _, _, codes = a.q("GRANT SELECT ON t TO alice")
        check("nosuper-grant", codes == ["42501"], str(codes))
        a.close()
    finally:
        srv.stop()


def t_nologin_and_unknown():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE nolog LOGIN PASSWORD 'x'")
        c.q("ALTER ROLE nolog NOLOGIN")
        c.close()
        try:
            Conn(srv.port, user="nolog")
            check("nologin-denied", False, "connected anyway")
        except RuntimeError as e:
            check("nologin-denied", "28000" in str(e), str(e))
        try:
            Conn(srv.port, user="nosuchuser")
            check("unknown-user", False, "connected anyway")
        except RuntimeError as e:
            check("unknown-user", "28P01" in str(e), str(e))
    finally:
        srv.stop()


def t_table_privs():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE ROLE bob LOGIN")
        c.q("CREATE TABLE t (a int)")
        c.q("INSERT INTO t VALUES (1), (2)")
        # default: no privileges
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("SELECT * FROM t")
        check("default-no-select", codes == ["42501"], str(codes))
        _, _, codes = a.q("INSERT INTO t VALUES (3)")
        check("default-no-insert", codes == ["42501"], str(codes))
        a.close()
        # grant SELECT
        c.q("GRANT SELECT ON t TO alice")
        a = Conn(srv.port, user="alice")
        _, rows, codes = a.q("SELECT * FROM t ORDER BY a")
        check("grant-select", rows == [("1",), ("2",)] and not codes, str((rows, codes)))
        _, _, codes = a.q("INSERT INTO t VALUES (3)")
        check("select-only-no-insert", codes == ["42501"], str(codes))
        _, _, codes = a.q("SELECT * FROM t FOR UPDATE")
        check("for-update-needs-update", codes == ["42501"], str(codes))
        a.close()
        # grant INSERT, UPDATE, DELETE
        c.q("GRANT INSERT, UPDATE, DELETE ON t TO alice")
        a = Conn(srv.port, user="alice")
        tags, _, codes = a.q("INSERT INTO t VALUES (3)")
        check("grant-insert", tags == ["INSERT 0 1"] and not codes, str((tags, codes)))
        tags, _, codes = a.q("UPDATE t SET a = 30 WHERE a = 3")
        check("grant-update", tags == ["UPDATE 1"] and not codes, str((tags, codes)))
        tags, _, codes = a.q("DELETE FROM t WHERE a = 30")
        check("grant-delete", tags == ["DELETE 1"] and not codes, str((tags, codes)))
        tags, _, codes = a.q("SELECT * FROM t FOR UPDATE")
        check("for-update-ok", tags == ["SELECT 2"] and not codes, str((tags, codes)))
        a.close()
        # revoke
        c.q("REVOKE INSERT ON t FROM alice")
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("INSERT INTO t VALUES (4)")
        check("revoke-insert", codes == ["42501"], str(codes))
        _, rows, codes = a.q("SELECT count(*) FROM t")
        check("select-still-ok", rows == [("2",)] and not codes, str((rows, codes)))
        a.close()
        # non-owner cannot grant
        b = Conn(srv.port, user="bob")
        _, _, codes = b.q("GRANT SELECT ON t TO bob")
        check("nonowner-grant", codes == ["42501"], str(codes))
        b.close()
        # owner can grant
        c.q("GRANT SELECT ON t TO bob")
        b = Conn(srv.port, user="bob")
        _, rows, codes = b.q("SELECT count(*) FROM t")
        check("owner-grant-ok", rows == [("2",)] and not codes, str((rows, codes)))
        b.close()
        c.close()
    finally:
        srv.stop()


def t_sequence_privs():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE SEQUENCE s START WITH 10")
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("SELECT nextval('s')")
        check("seq-no-usage", codes == ["42501"], str(codes))
        a.close()
        c.q("GRANT USAGE ON SEQUENCE s TO alice")
        a = Conn(srv.port, user="alice")
        _, rows, codes = a.q("SELECT nextval('s')")
        check("seq-usage", rows == [("10",)] and not codes, str((rows, codes)))
        _, rows, codes = a.q("SELECT currval('s')")
        check("seq-currval", rows == [("10",)] and not codes, str((rows, codes)))
        a.close()
        c.q("REVOKE USAGE ON SEQUENCE s FROM alice")
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("SELECT nextval('s')")
        check("seq-revoked", codes == ["42501"], str(codes))
        a.close()
        # sequence USAGE via table default: alice has INSERT on t but no USAGE on s
        c.q("CREATE TABLE d (id bigint DEFAULT nextval('s'), v int)")
        c.q("GRANT INSERT ON d TO alice")
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("INSERT INTO d (v) VALUES (1)")
        check("default-nextval-needs-usage", codes == ["42501"], str(codes))
        a.close()
        c.q("GRANT USAGE ON SEQUENCE s TO alice")
        a = Conn(srv.port, user="alice")
        tags, _, codes = a.q("INSERT INTO d (v) VALUES (1)")
        check("default-nextval-ok", tags == ["INSERT 0 1"] and not codes, str((tags, codes)))
        a.close()
        c.close()
    finally:
        srv.stop()


def t_ownership():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE ROLE bob LOGIN")
        c.q("CREATE TABLE t (a int)")
        a = Conn(srv.port, user="alice")
        tags, _, codes = a.q("CREATE TABLE at (a int)")
        check("alice-create-table", tags == ["CREATE TABLE"] and not codes, str((tags, codes)))
        _, _, codes = a.q("DROP TABLE t")
        check("nonowner-drop", codes == ["42501"], str(codes))
        _, _, codes = a.q("ALTER TABLE t ADD COLUMN b int")
        check("nonowner-alter", codes == ["42501"], str(codes))
        _, _, codes = a.q("CREATE INDEX i ON t (a)")
        check("nonowner-create-index", codes == ["42501"], str(codes))
        a.close()
        # owner transfer
        c.q("ALTER TABLE t OWNER TO alice")
        a = Conn(srv.port, user="alice")
        tags, _, codes = a.q("DROP TABLE t")
        check("new-owner-drop", tags == ["DROP TABLE"] and not codes, str((tags, codes)))
        a.close()
        # cannot drop role owning objects
        _, _, codes = c.q("DROP ROLE alice")
        check("drop-role-owns-table", codes == ["2BP01"], str(codes))
        # bootstrap postgres cannot be dropped
        _, _, codes = c.q("DROP ROLE postgres")
        check("drop-postgres", codes == ["42501"], str(codes))
        c.close()
    finally:
        srv.stop()


def t_connect_priv():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.close()
        Conn(srv.port, user="alice").close()  # default allow
        c = Conn(srv.port)
        c.q("REVOKE CONNECT ON DATABASE FROM alice")
        c.close()
        try:
            Conn(srv.port, user="alice")
            check("revoke-connect", False, "connected anyway")
        except RuntimeError as e:
            check("revoke-connect", "42501" in str(e), str(e))
        # raw check: the denial must arrive as Error with NO AuthenticationOk
        # before it (authorization precedes AuthenticationOk, like PG).
        s = socket.create_connection((HOST, srv.port), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00alice\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        t = s.recv(1)
        (ln,) = struct.unpack("!i", _read_exact(s, 4))
        p = _read_exact(s, ln - 4)
        s.close()
        if t == b"R":
            (auth,) = struct.unpack("!i", p[:4])
            check("revoke-connect-no-authok", False, "got AuthenticationOk first")
        else:
            check("revoke-connect-no-authok", t == b"E" and err_code(p) == "42501",
                  "first=%r code=%s" % (t, err_code(p) if t == b"E" else "?"))
        c = Conn(srv.port)
        c.q("GRANT CONNECT ON DATABASE TO alice")
        c.close()
        Conn(srv.port, user="alice").close()
        check("grant-connect", True)
    finally:
        srv.stop()


def t_connlimit():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN CONNECTION LIMIT 1")
        c.close()
        a = Conn(srv.port, user="alice")
        try:
            Conn(srv.port, user="alice", timeout=5)
            check("connlimit", False, "second connection allowed")
        except RuntimeError as e:
            check("connlimit", "53300" in str(e), str(e))
        a.close()
        # slot freed after disconnect (give the server a beat to reap it)
        ok = False
        for _ in range(40):
            try:
                Conn(srv.port, user="alice", timeout=5).close()
                ok = True
                break
            except RuntimeError:
                time.sleep(0.1)
        check("connlimit-released", ok)
    finally:
        srv.stop()


def t_catalogs():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN PASSWORD 'pw'")
        _, rows, _ = c.q("SELECT rolname FROM pg_roles ORDER BY rolname")
        check("pg-roles", ("alice",) in rows and ("postgres",) in rows, str(rows))
        _, rows, _ = c.q("SELECT usename, usesuper FROM pg_user WHERE usename='postgres'")
        check("pg-user", rows == [("postgres", "t")], str(rows))
        c.close()
        a = Conn(srv.port, user="alice")
        _, rows, _ = a.q("SELECT rolname, rolpassword FROM pg_authid WHERE rolname='alice'")
        check("authid-masked", rows == [("alice", "********")], str(rows))
        a.close()
    finally:
        srv.stop()


def t_recovery():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN PASSWORD 'pw'")
        c.q("CREATE TABLE t (a int)")
        c.q("INSERT INTO t VALUES (1)")
        c.q("GRANT SELECT ON t TO alice")
        c.q("REVOKE CONNECT ON DATABASE FROM alice")
        c.q("GRANT CONNECT ON DATABASE TO alice")
        c.close()
    finally:
        srv.proc.terminate()
        srv.proc.wait(timeout=5)
    # reboot on the same data dir
    srv2 = Server()
    srv2.proc.terminate()
    srv2.proc.wait(timeout=5)
    shutil.rmtree(srv2.datadir, ignore_errors=True)
    srv2.datadir = srv.datadir
    srv2.proc = subprocess.Popen(
        [BIN, "--port", str(srv2.port), "--data-dir", srv2.datadir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                socket.create_connection((HOST, srv2.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        a = Conn(srv2.port, user="alice")
        _, rows, codes = a.q("SELECT * FROM t")
        check("recovery-select", rows == [("1",)] and not codes, str((rows, codes)))
        _, _, codes = a.q("INSERT INTO t VALUES (2)")
        check("recovery-still-no-insert", codes == ["42501"], str(codes))
        a.close()
        # role + password survived: SCRAM would need them; check the catalog
        c = Conn(srv2.port)
        _, rows, _ = c.q("SELECT rolcanlogin FROM pg_roles WHERE rolname='alice'")
        check("recovery-role", rows == [("t",)], str(rows))
        c.close()
    finally:
        srv2.proc.terminate()
        try:
            srv2.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            srv2.proc.kill()
        shutil.rmtree(srv2.datadir, ignore_errors=True)


def t_recovery_v11():
    # memberships, column grants, and VALID UNTIL survive CHECKPOINT +
    # restart (checkpoint image path, not just WAL replay).
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE grp NOLOGIN")
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE ROLE tmp LOGIN VALID UNTIL '2030-06-01 12:00:00'")
        c.q("CREATE TABLE t (a int, b int)")
        c.q("INSERT INTO t VALUES (1, 2)")
        c.q("GRANT grp TO alice")
        c.q("GRANT SELECT (a) ON t TO grp")
        c.q("CHECKPOINT")
        datadir = srv.datadir
        c.close()
    finally:
        srv.proc.terminate()
        srv.proc.wait(timeout=5)
    srv2 = Server()
    srv2.proc.terminate()
    srv2.proc.wait(timeout=5)
    shutil.rmtree(srv2.datadir, ignore_errors=True)
    srv2.datadir = datadir
    srv2.proc = subprocess.Popen(
        [BIN, "--port", str(srv2.port), "--data-dir", srv2.datadir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                socket.create_connection((HOST, srv2.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        a = Conn(srv2.port, user="alice")
        _, rows, codes = a.q("SELECT a FROM t")
        check("recov-col-select", rows == [("1",)] and not codes, str((rows, codes)))
        _, _, codes = a.q("SELECT b FROM t")
        check("recov-col-deny", codes == ["42501"], str(codes))
        _, rows, _ = a.q("SELECT count(*) FROM pg_auth_members")
        check("recov-members", rows == [("1",)], str(rows))
        a.close()
        c = Conn(srv2.port)
        _, rows, _ = c.q("SELECT rolvaliduntil FROM pg_roles WHERE rolname='tmp'")
        check("recov-validuntil", rows == [("2030-06-01 12:00:00",)], str(rows))
        c.close()
    finally:
        srv2.proc.terminate()
        try:
            srv2.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            srv2.proc.kill()
        shutil.rmtree(srv.datadir, ignore_errors=True)


def t_scram():
    # phase 1: trust mode, create password roles
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN PASSWORD 's3cret'")
        c.q("CREATE ROLE nopw LOGIN")
        c.q("CREATE ROLE nolog LOGIN PASSWORD 's3cret'")
        c.q("ALTER ROLE nolog NOLOGIN")
        c.q("ALTER ROLE postgres PASSWORD 'rootpw'")
        datadir = srv.datadir
        c.close()
    finally:
        srv.proc.terminate()
        srv.proc.wait(timeout=5)
    # phase 2: SCRAM mode on the same data dir
    port = alloc_port()
    env = dict(os.environ, RUSTGRES_AUTH="scram-sha-256")
    proc = subprocess.Popen(
        [BIN, "--port", str(port), "--data-dir", datadir],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                socket.create_connection((HOST, port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        s, ok, code = scram_connect(port, "alice", "s3cret")
        check("scram-ok", ok, code)
        if s:
            s.close()
        _, ok, code = scram_connect(port, "alice", "wrong")
        check("scram-wrong-password", not ok and code == "28P01", code)
        _, ok, code = scram_connect(port, "alice", "s3cret", tamper_nonce=True)
        check("scram-tampered-nonce", not ok and code == "28P01", code)
        _, ok, code = scram_connect(port, "nosuchuser", "whatever")
        check("scram-unknown-user", not ok and code == "28P01", code)
        _, ok, code = scram_connect(port, "nopw", "")
        check("scram-no-password", not ok and code == "28P01", code)
        _, ok, code = scram_connect(port, "nolog", "s3cret")
        check("scram-nologin", not ok and code == "28000", code)
        s, ok, code = scram_connect(port, "postgres", "rootpw")
        check("scram-postgres", ok, code)
        if s:
            s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(datadir, ignore_errors=True)


def t_membership():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE readers NOLOGIN")
        c.q("CREATE ROLE devs NOLOGIN")
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE ROLE bob LOGIN")
        c.q("CREATE TABLE t (a int, b int)")
        c.q("INSERT INTO t VALUES (1, 2)")
        # basic grant + inheritance
        tags, _, codes = c.q("GRANT readers TO alice")
        check("grant-role", tags == ["GRANT"] and not codes, str((tags, codes)))
        c.q("GRANT SELECT ON t TO readers")
        # transitive: bob -> devs -> readers
        c.q("GRANT readers TO devs")
        c.q("GRANT devs TO bob")
        # duplicate grant is a no-op
        tags, _, codes = c.q("GRANT readers TO alice")
        check("grant-role-dup", tags == ["GRANT"] and not codes, str((tags, codes)))
        _, rows, _ = c.q("SELECT count(*) FROM pg_auth_members")
        check("members-count", rows == [("3",)], str(rows))
        # self-membership rejected
        _, _, codes = c.q("GRANT readers TO readers")
        check("grant-self", codes == ["42501"], str(codes))
        # cycle rejected
        _, _, codes = c.q("GRANT alice TO readers")
        check("grant-cycle", codes == ["42501"], str(codes))
        # missing roles
        _, _, codes = c.q("GRANT nosuch TO alice")
        check("grant-role-missing", codes == ["42704"], str(codes))
        _, _, codes = c.q("GRANT readers TO nosuch")
        check("grant-member-missing", codes == ["42704"], str(codes))
        # revoking a non-edge is a silent no-op
        tags, _, codes = c.q("REVOKE readers FROM bob")
        check("revoke-role-missing", tags == ["REVOKE"] and not codes, str((tags, codes)))
        c.close()
        # alice inherits SELECT via readers; bob transitively via devs
        a = Conn(srv.port, user="alice")
        _, rows, codes = a.q("SELECT a, b FROM t")
        check("member-inherit", rows == [("1", "2")] and not codes, str((rows, codes)))
        a.close()
        b = Conn(srv.port, user="bob")
        _, rows, codes = b.q("SELECT a, b FROM t")
        check("member-transitive", rows == [("1", "2")] and not codes, str((rows, codes)))
        # bob (non-superuser) cannot administer memberships
        _, _, codes = b.q("GRANT readers TO bob")
        check("nosuper-grant-role", codes == ["42501"], str(codes))
        _, _, codes = b.q("REVOKE devs FROM bob")
        check("nosuper-revoke-role", codes == ["42501"], str(codes))
        b.close()
        # REVOKE kills inheritance
        c = Conn(srv.port)
        tags, _, codes = c.q("REVOKE readers FROM alice")
        check("revoke-role", tags == ["REVOKE"] and not codes, str((tags, codes)))
        c.close()
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("SELECT a FROM t")
        check("revoke-role-deny", codes == ["42501"], str(codes))
        a.close()
        # pg_auth_members exposes the edges (roleid/member/grantor oid
        # stand-ins, admin_option f)
        c = Conn(srv.port)
        _, rows, _ = c.q("SELECT admin_option FROM pg_auth_members")
        check("auth-members-admin", rows and all(r == ("f",) for r in rows), str(rows))
        # DROP ROLE cleans up membership edges
        c.q("DROP ROLE readers")
        _, rows, _ = c.q("SELECT count(*) FROM pg_auth_members")
        check("drop-role-cleans-members", rows == [("1",)], str(rows))
        c.close()
        b = Conn(srv.port, user="bob")
        _, _, codes = b.q("SELECT a FROM t")
        check("drop-role-kills-inherit", codes == ["42501"], str(codes))
        b.close()
    finally:
        srv.stop()


def t_valid_until():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        tags, _, codes = c.q("CREATE ROLE future LOGIN VALID UNTIL '2030-01-02 03:04:05'")
        check("valid-until", tags == ["CREATE ROLE"] and not codes, str((tags, codes)))
        _, rows, _ = c.q("SELECT rolvaliduntil FROM pg_roles WHERE rolname='future'")
        check("rolvaliduntil", rows == [("2030-01-02 03:04:05",)], str(rows))
        _, rows, _ = c.q("SELECT rolvaliduntil FROM pg_authid WHERE rolname='future'")
        check("authid-validuntil", rows == [("2030-01-02 03:04:05",)], str(rows))
        _, rows, _ = c.q("SELECT valuntil FROM pg_user WHERE usename='future'")
        check("pguser-valuntil", rows == [("2030-01-02 03:04:05",)], str(rows))
        # ALTER ROLE can set / clear expiry
        c.q("ALTER ROLE future VALID UNTIL '2031-05-06 07:08:09'")
        _, rows, _ = c.q("SELECT rolvaliduntil FROM pg_roles WHERE rolname='future'")
        check("alter-valid-until", rows == [("2031-05-06 07:08:09",)], str(rows))
        c.q("ALTER ROLE future VALID UNTIL 'infinity'")
        _, rows, _ = c.q("SELECT rolvaliduntil FROM pg_roles WHERE rolname='future'")
        check("alter-valid-until-infinity", rows == [(None,)], str(rows))
        # invalid timestamps rejected
        _, _, codes = c.q("CREATE ROLE bad LOGIN VALID UNTIL 'not-a-date'")
        check("valid-until-bad", codes == ["22008"], str(codes))
        _, _, codes = c.q("ALTER ROLE future VALID UNTIL '32-13-99'")
        check("alter-valid-until-bad", codes == ["22008"], str(codes))
        # trust mode ignores expiry for connection purposes
        c.q("CREATE ROLE expired LOGIN VALID UNTIL '2001-01-01 00:00:00'")
        c.close()
        e = Conn(srv.port, user="expired")
        _, rows, codes = e.q("SELECT 1")
        check("trust-ignores-expiry", rows == [("1",)] and not codes, str((rows, codes)))
        e.close()
    finally:
        srv.stop()
    # SCRAM mode: expired password rejected, live one accepted
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE exp LOGIN PASSWORD 'pw' VALID UNTIL '2001-01-01 00:00:00'")
        c.q("CREATE ROLE live LOGIN PASSWORD 'pw' VALID UNTIL '2030-01-01 00:00:00'")
        datadir = srv.datadir
        c.close()
    finally:
        srv.proc.terminate()
        srv.proc.wait(timeout=5)
    port = alloc_port()
    env = dict(os.environ, RUSTGRES_AUTH="scram-sha-256")
    proc = subprocess.Popen(
        [BIN, "--port", str(port), "--data-dir", datadir],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                socket.create_connection((HOST, port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        _, ok, code = scram_connect(port, "exp", "pw")
        check("scram-expired", not ok and code == "28P01", str((ok, code)))
        s, ok, code = scram_connect(port, "live", "pw")
        check("scram-unexpired", ok, str((ok, code)))
        if s:
            s.close()
    finally:
        proc.terminate()
        proc.wait(timeout=5)
        shutil.rmtree(datadir, ignore_errors=True)


def t_column_privs():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE ROLE grp NOLOGIN")
        c.q("GRANT grp TO alice")
        c.q("CREATE TABLE t (a int, b int, c int)")
        c.q("INSERT INTO t VALUES (1, 2, 3)")
        # column grants, incl. through a group role
        tags, _, codes = c.q("GRANT SELECT (a, b) ON t TO grp")
        check("grant-col-select", tags == ["GRANT"] and not codes, str((tags, codes)))
        tags, _, codes = c.q("GRANT UPDATE (a), INSERT (a, b) ON t TO alice")
        check("grant-col-multi", tags == ["GRANT"] and not codes, str((tags, codes)))
        # unknown column / bad privilege placement
        _, _, codes = c.q("GRANT SELECT (zz) ON t TO alice")
        check("grant-col-unknown", codes == ["42703"], str(codes))
        _, _, codes = c.q("GRANT DELETE (a) ON t TO alice")
        check("grant-col-delete", codes == ["42601"], str(codes))
        _, _, codes = c.q("GRANT ALL (a) ON t TO alice")
        check("grant-col-all", codes == ["42601"], str(codes))
        c.close()
        a = Conn(srv.port, user="alice")
        _, rows, codes = a.q("SELECT a, b FROM t")
        check("col-select-ok", rows == [("1", "2")] and not codes, str((rows, codes)))
        _, _, codes = a.q("SELECT c FROM t")
        check("col-select-deny", codes == ["42501"], str(codes))
        _, _, codes = a.q("SELECT * FROM t")
        check("col-star-deny", codes == ["42501"], str(codes))
        _, _, codes = a.q("SELECT a FROM t WHERE c = 3")
        check("col-where-deny", codes == ["42501"], str(codes))
        _, _, codes = a.q("SELECT a FROM t WHERE b = 2")
        check("col-where-ok", not codes, str(codes))
        # join with qualified refs
        _, _, codes = a.q("SELECT t.a FROM t t")
        check("col-qual-ok", not codes, str(codes))
        # INSERT with column list: needs INSERT on each listed column
        tags, _, codes = a.q("INSERT INTO t (a, b) VALUES (4, 5)")
        check("col-insert-ok", tags == ["INSERT 0 1"] and not codes, str((tags, codes)))
        _, _, codes = a.q("INSERT INTO t (a, c) VALUES (4, 5)")
        check("col-insert-deny", codes == ["42501"], str(codes))
        # UPDATE: needs UPDATE on each SET column
        tags, _, codes = a.q("UPDATE t SET a = 10 WHERE a = 1")
        check("col-update-ok", tags == ["UPDATE 1"] and not codes, str((tags, codes)))
        _, _, codes = a.q("UPDATE t SET b = 10 WHERE a = 10")
        check("col-update-deny", codes == ["42501"], str(codes))
        a.close()
        # REVOKE a column grant
        c = Conn(srv.port)
        tags, _, codes = c.q("REVOKE SELECT (b) ON t FROM grp")
        check("revoke-col", tags == ["REVOKE"] and not codes, str((tags, codes)))
        c.close()
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("SELECT b FROM t")
        check("revoke-col-deny", codes == ["42501"], str(codes))
        _, rows, codes = a.q("SELECT a FROM t")
        check("revoke-col-keeps-a", rows and not codes, str((rows, codes)))
        a.close()
    finally:
        srv.stop()


def t_ddl_audit():
    srv = fresh_server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE alice LOGIN")
        c.q("CREATE TABLE t (a int)")
        c.q("CREATE VIEW v AS SELECT * FROM t")
        c.close()
        a = Conn(srv.port, user="alice")
        _, _, codes = a.q("ANALYZE t")
        check("analyze-deny", codes == ["42501"], str(codes))
        _, _, codes = a.q("VACUUM t")
        check("vacuum-deny", codes == ["42501"], str(codes))
        _, _, codes = a.q("CREATE OR REPLACE VIEW v AS SELECT * FROM t")
        check("or-replace-view-deny", codes == ["42501"], str(codes))
        # plain ANALYZE/VACUUM touch nothing owned: no error
        tags, _, codes = a.q("ANALYZE")
        check("analyze-nothing", tags == ["ANALYZE"] and not codes, str((tags, codes)))
        tags, _, codes = a.q("VACUUM")
        check("vacuum-nothing", tags == ["VACUUM"] and not codes, str((tags, codes)))
        a.close()
        # owner paths still work
        c = Conn(srv.port)
        tags, _, codes = c.q("ANALYZE t")
        check("analyze-owner", tags == ["ANALYZE"] and not codes, str((tags, codes)))
        tags, _, codes = c.q("VACUUM t")
        check("vacuum-owner", tags == ["VACUUM"] and not codes, str((tags, codes)))
        tags, _, codes = c.q("CREATE OR REPLACE VIEW v AS SELECT a FROM t")
        check("or-replace-view-owner", tags == ["CREATE VIEW"] and not codes, str((tags, codes)))
        c.close()
    finally:
        srv.stop()


def main():
    if not os.path.exists(BIN):
        print(f"build the server first: {BIN} missing")
        sys.exit(2)
    tests = [
        t_role_lifecycle,
        t_role_perm_denied,
        t_nologin_and_unknown,
        t_table_privs,
        t_sequence_privs,
        t_ownership,
        t_connect_priv,
        t_connlimit,
        t_catalogs,
        t_recovery,
        t_recovery_v11,
        t_scram,
        t_membership,
        t_valid_until,
        t_column_privs,
        t_ddl_audit,
    ]
    for t in tests:
        print(f"== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            failed.append(t.__name__)
            print(f"  FAIL: {t.__name__} raised {e!r}")
            import traceback
            traceback.print_exc()
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    if failed:
        print("failed:", failed)
        sys.exit(1)


if __name__ == "__main__":
    main()
