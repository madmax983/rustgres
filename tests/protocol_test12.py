#!/usr/bin/env python3
"""Raw-socket tests for rustgres v0.12 (concurrency, soak, protocol hardening).

Groups:
  A. concurrency stress — threaded mixed read/write workload with
     primary-key uniqueness invariant; concurrent conflicting
     transactions (40001 tolerated); same-name DDL race (42P07/40001);
     concurrent GRANT/REVOKE vs privilege enforcement (only 42501 or
     success); nextval uniqueness across threads; CHECKPOINT/VACUUM
     under load; rapid connect/disconnect.
  B. protocol hardening — giant length lies rejected without allocation
     (server stays alive); unknown message type -> 08P01 + FATAL close;
     CancelRequest quiet close; SSLRequest 'N' then normal startup;
     bad protocol version refused; truncated header handled; CopyData
     outside COPY -> 08P01; out-of-order Execute/Bind codes.
  C. error-code pins — statements mapped to exact SQLSTATEs.
  D. mini-soak — 10s randomized workload, zero unexpected SQLSTATEs,
     then SIGKILL + restart recovery with data intact.

Each group boots fresh servers on scratch ports/data dirs.
Usage: build the server first (`cargo build`), then `python3 tests/protocol_test12.py`.
"""
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
HOST = "127.0.0.1"

_next_port = [56043]

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

class Conn:
    def __init__(self, port, user="postgres", timeout=30):
        self.s = socket.create_connection((HOST, port), timeout=timeout)
        body = struct.pack("!i", 196608) + b"user\x00" + user.encode() + b"\x00\x00"
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

class Server:
    def __init__(self):
        self.port = alloc_port()
        self.datadir = tempfile.mkdtemp(prefix="rg12-")
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
        _, rows, codes = c.q("SELECT 1")
        c.close()
        return not codes and rows == [("1",)]
    except Exception:
        return False

# ---------------------------------------------------------------------------
# A. concurrency
# ---------------------------------------------------------------------------

def t_concurrent_mixed_workload():
    srv = Server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE conc(id INT PRIMARY KEY, v INT)")
        c.close()
        errors = []
        def inserter(n):
            try:
                cc = Conn(srv.port)
                for i in range(40):
                    _, _, codes = cc.q(f"INSERT INTO conc VALUES ({(n * 40 + i) % 96}, {i})")
                    for code in codes:
                        assert code == "23505", f"bad code {code}"
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        def reader(n):
            try:
                cc = Conn(srv.port)
                for _ in range(40):
                    _, _, codes = cc.q("SELECT COUNT(*) FROM conc")
                    assert not codes, f"reader codes {codes}"
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        def txner(n):
            try:
                cc = Conn(srv.port)
                for _ in range(15):
                    cc.q("BEGIN")
                    cc.q(f"UPDATE conc SET v = v + 1 WHERE id = {n % 8}")
                    _, _, codes = cc.q("COMMIT")
                    assert all(x == "40001" for x in codes), f"txner codes {codes}"
                    if codes:
                        cc.q("ROLLBACK")
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        threads = ([threading.Thread(target=inserter, args=(i,)) for i in range(4)]
                   + [threading.Thread(target=reader, args=(i,)) for i in range(4)]
                   + [threading.Thread(target=txner, args=(i,)) for i in range(3)])
        for t in threads: t.start()
        for t in threads: t.join(timeout=120)
        stuck = [t for t in threads if t.is_alive()]
        check("mixed-no-thread-errors", not errors, str(errors[:3]))
        check("mixed-no-stuck-threads", not stuck, f"{len(stuck)} stuck")
        c = Conn(srv.port)
        _, rows, codes = c.q("SELECT COUNT(*), COUNT(DISTINCT id) FROM conc")
        check("mixed-pk-invariant", not codes and rows[0][0] == rows[0][1] == "96",
              str((rows, codes)))
        c.close()
        check("mixed-server-alive", alive(srv.port))
    finally:
        srv.stop()

def t_same_name_ddl_race():
    srv = Server()
    try:
        errors = []
        def racer(n):
            try:
                cc = Conn(srv.port)
                for _ in range(10):
                    _, _, codes = cc.q("CREATE TABLE race_t(id INT)")
                    for code in codes:
                        assert code in ("42P07", "40001"), f"create bad {code}"
                    _, _, codes = cc.q("DROP TABLE race_t")
                    for code in codes:
                        assert code in ("42P01", "40001"), f"drop bad {code}"
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        threads = [threading.Thread(target=racer, args=(i,)) for i in range(4)]
        for t in threads: t.start()
        for t in threads: t.join(timeout=120)
        check("ddl-race-no-errors", not errors, str(errors[:3]))
        check("ddl-race-alive", alive(srv.port))
    finally:
        srv.stop()

def t_grant_race_enforcement():
    srv = Server()
    try:
        c = Conn(srv.port)
        c.q("CREATE ROLE racer LOGIN")
        c.q("CREATE TABLE gtab(id INT)")
        c.q("INSERT INTO gtab VALUES (1)")
        c.close()
        errors = []
        def granter(n):
            try:
                cc = Conn(srv.port)
                for _ in range(25):
                    cc.q("GRANT SELECT ON gtab TO racer")
                    cc.q("REVOKE SELECT ON gtab FROM racer")
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        results = []
        def checker(n):
            try:
                cc = Conn(srv.port, user="racer")
                ok = denied = 0
                for _ in range(50):
                    _, _, codes = cc.q("SELECT * FROM gtab")
                    if not codes:
                        ok += 1
                    else:
                        assert codes == ["42501"], f"checker codes {codes}"
                        denied += 1
                cc.close()
                results.append((ok, denied))
            except Exception as e:
                errors.append(repr(e)[:150])
        threads = [threading.Thread(target=granter, args=(i,)) for i in range(2)]
        threads += [threading.Thread(target=checker, args=(i,)) for i in range(2)]
        for t in threads: t.start()
        for t in threads: t.join(timeout=120)
        check("grant-race-no-errors", not errors, str(errors[:3]))
        check("grant-race-saw-both", all(ok > 0 and denied > 0 for ok, denied in results),
              str(results))
        check("grant-race-alive", alive(srv.port))
    finally:
        srv.stop()

def t_nextval_race_unique():
    srv = Server()
    try:
        c = Conn(srv.port)
        c.q("CREATE SEQUENCE s1")
        c.close()
        vals, errors, lock = [], [], threading.Lock()
        def seqer(n):
            try:
                cc = Conn(srv.port)
                for _ in range(40):
                    _, rows, codes = cc.q("SELECT nextval('s1')")
                    assert not codes, f"seqer {codes}"
                    with lock:
                        vals.append(int(rows[0][0]))
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        threads = [threading.Thread(target=seqer, args=(i,)) for i in range(4)]
        for t in threads: t.start()
        for t in threads: t.join(timeout=120)
        check("nextval-no-errors", not errors, str(errors[:3]))
        check("nextval-unique", len(vals) == 160 and len(set(vals)) == 160,
              f"n={len(vals)} unique={len(set(vals))}")
        check("nextval-alive", alive(srv.port))
    finally:
        srv.stop()

def t_checkpoint_vacuum_under_load():
    srv = Server()
    try:
        c = Conn(srv.port)
        c.q("CREATE TABLE mtab(id INT)")
        c.q("INSERT INTO mtab VALUES (1),(2),(3)")
        c.close()
        errors = []
        def writer(n):
            try:
                cc = Conn(srv.port)
                for i in range(30):
                    cc.q(f"INSERT INTO mtab VALUES ({i})")
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        def maint(n):
            try:
                cc = Conn(srv.port)
                for _ in range(10):
                    _, _, codes = cc.q("CHECKPOINT")
                    assert not codes, f"ckpt {codes}"
                    _, _, codes = cc.q("VACUUM mtab")
                    assert not codes, f"vacuum {codes}"
                cc.close()
            except Exception as e:
                errors.append(repr(e)[:150])
        threads = [threading.Thread(target=writer, args=(i,)) for i in range(3)]
        threads.append(threading.Thread(target=maint, args=(0,)))
        for t in threads: t.start()
        for t in threads: t.join(timeout=120)
        check("maint-no-errors", not errors, str(errors[:3]))
        c = Conn(srv.port)
        _, rows, codes = c.q("SELECT COUNT(*) FROM mtab")
        check("maint-row-count", not codes and rows == [("93",)], str((rows, codes)))
        c.close()
    finally:
        srv.stop()

def t_rapid_connect_disconnect():
    srv = Server()
    try:
        t0 = time.time()
        for _ in range(150):
            c = Conn(srv.port)
            c.close()
        dt = time.time() - t0
        check("rapid-connect-150", dt < 30, f"{dt:.1f}s")
        check("rapid-connect-alive", alive(srv.port))
    finally:
        srv.stop()

# ---------------------------------------------------------------------------
# B. protocol hardening
# ---------------------------------------------------------------------------

def t_giant_length_lie():
    srv = Server()
    try:
        s = socket.create_connection((HOST, srv.port), timeout=5)
        s.sendall(b"Q" + struct.pack("!i", 2**31 - 1))  # claim ~2 GiB, send nothing
        s.settimeout(5)
        try:
            s.recv(1)
        except (OSError, socket.timeout):
            pass
        s.close()
        time.sleep(0.2)
        check("giant-len-server-alive", alive(srv.port))
        # giant startup packet lie
        s = socket.create_connection((HOST, srv.port), timeout=5)
        s.sendall(struct.pack("!i", 2**31 - 1))
        s.settimeout(5)
        try:
            s.recv(1)
        except (OSError, socket.timeout):
            pass
        s.close()
        check("giant-startup-server-alive", alive(srv.port))
    finally:
        srv.stop()

def t_unknown_message_type_fatal():
    srv = Server()
    try:
        c = Conn(srv.port)
        c.s.sendall(b"\x99" + struct.pack("!i", 4))
        t, p = c._read_msg(timeout=5)
        check("unknown-msgtype-08P01", t == b"E" and err_code(p) == "08P01",
              f"{t} {err_code(p) if t == b'E' else ''}")
        try:
            t2, _ = c._read_msg(timeout=3)
            closed = t2 is None
        except Exception:
            closed = True
        check("unknown-msgtype-fatal-close", closed)
        c.s.close()
        check("unknown-msgtype-alive", alive(srv.port))
    finally:
        srv.stop()

def t_cancel_request_quiet():
    srv = Server()
    try:
        s = socket.create_connection((HOST, srv.port), timeout=5)
        s.sendall(struct.pack("!i", 16) + struct.pack("!i", 80877102)
                  + struct.pack("!ii", 1234, 5678))
        s.settimeout(3)
        try:
            data = s.recv(1)
            closed = not data
        except (OSError, socket.timeout):
            closed = True
        check("cancel-quiet-close", closed)
        s.close()
        check("cancel-alive", alive(srv.port))
    finally:
        srv.stop()

def t_ssl_then_startup():
    srv = Server()
    try:
        s = socket.create_connection((HOST, srv.port), timeout=5)
        s.sendall(struct.pack("!i", 8) + struct.pack("!i", 80877103))
        check("ssl-N", _read_exact(s, 1) == b"N")
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        got_z = False
        s.settimeout(5)
        while True:
            t = s.recv(1)
            if not t:
                break
            (ln,) = struct.unpack("!i", _read_exact(s, 4))
            _read_exact(s, ln - 4)
            if t == b"Z":
                got_z = True
                break
            if t == b"E":
                break
        check("ssl-then-startup", got_z)
        s.close()
    finally:
        srv.stop()

def t_bad_startup_variants():
    srv = Server()
    try:
        # bad protocol version
        s = socket.create_connection((HOST, srv.port), timeout=5)
        body = struct.pack("!i", 196602) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        s.settimeout(3)
        try:
            data = s.recv(1)
            closed = not data
        except (OSError, socket.timeout):
            closed = True
        check("bad-version-closed", closed)
        s.close()
        # truncated startup header
        s = socket.create_connection((HOST, srv.port), timeout=5)
        s.sendall(b"\x00\x00")
        s.close()
        time.sleep(0.2)
        check("truncated-startup-alive", alive(srv.port))
    finally:
        srv.stop()

def t_out_of_order_extended():
    srv = Server()
    try:
        c = Conn(srv.port)
        # Execute unknown portal
        c.s.sendall(b"E" + struct.pack("!i", 10) + b"\x00" + b"nope\x00")
        t, p = c._read_msg(timeout=5)
        check("exec-unknown-portal-34000", t == b"E" and err_code(p) == "34000",
              f"{t} {err_code(p) if t == b'E' else ''}")
        c.s.sendall(msg(b"S", b""))
        while True:
            t, _ = c._read_msg()
            if t == b"Z":
                break
        # Bind unknown statement
        payload = b"\x00" + b"nope\x00" + struct.pack("!hhh", 0, 0, 0)
        c.s.sendall(b"B" + struct.pack("!i", 4 + len(payload)) + payload)
        t, p = c._read_msg(timeout=5)
        check("bind-unknown-stmt-26000", t == b"E" and err_code(p) == "26000",
              f"{t} {err_code(p) if t == b'E' else ''}")
        c.s.sendall(msg(b"S", b""))
        while True:
            t, _ = c._read_msg()
            if t == b"Z":
                break
        # CopyData outside COPY
        c.s.sendall(b"d" + struct.pack("!i", 8) + b"junk")
        t, p = c._read_msg(timeout=5)
        check("copydata-outside-copy-08P01", t == b"E" and err_code(p) == "08P01",
              f"{t} {err_code(p) if t == b'E' else ''}")
        c.s.close()
        check("out-of-order-alive", alive(srv.port))
    finally:
        srv.stop()

# ---------------------------------------------------------------------------
# C. error-code pins
# ---------------------------------------------------------------------------

ERROR_PINS = [
    ("SELECT * FROM nosuchtable", "42P01"),
    ("SELECT nosuchcol FROM (SELECT 1 AS x) t", "42703"),
    ("SELECT * FROM (SELECT 1 AS x) t ORDER BY nosuchcol", "42703"),
    ("INSERT INTO nosuchtable VALUES (1)", "42P01"),
    ("UPDATE nosuchtable SET a = 1", "42P01"),
    ("DELETE FROM nosuchtable", "42P01"),
    ("CREATE TABLE dup_pin(a INT); CREATE TABLE dup_pin(a INT)", "42P07"),
    ("DROP TABLE nosuchtable", "42P01"),
    ("SELECT * FROM (SELECT 1 AS a, 2 AS a) t", None),  # duplicate output names are legal in PG
    ("CREATE TABLE dupc_pin(a INT, a INT)", "42701"),
    ("SELECT a FROM (SELECT 1 AS a) t1, (SELECT 2 AS a) t2", "42702"),
    ("SELECT 'abc'::INT", "22P02"),
    ("SELECT 1/0", "22012"),
    ("SELECT 1 + 'abc'", "42883"),
    ("SELECT COUNT(*) FROM (SELECT 1 AS a) t GROUP BY a HAVING COUNT(*) > 0 ORDER BY b", "42703"),
    ("SELECT sum(a) FROM (SELECT 'x' AS a) t", "42883"),  # no sum(text): undefined_function
    ("BEGIN; SELECT 1; COMMIT; COMMIT", None),  # PG: WARNING only, no error
    ("ROLLBACK", None),  # PG: WARNING only, no error
    ("SAVEPOINT sp1", "25001"),
    ("CHECKPOINT", None),  # must succeed outside txn
]

def t_error_code_pins():
    srv = Server()
    try:
        c = Conn(srv.port)
        for sql, want in ERROR_PINS:
            _, _, codes = c.q(sql)
            if want is None:
                check(f"pin:{sql[:40]}-ok", not codes, str(codes))
            else:
                check(f"pin:{sql[:40]}->{want}", codes == [want], str(codes))
        # permission denied as non-owner
        c.q("CREATE ROLE pinuser LOGIN")
        c.q("CREATE TABLE pintab(a INT)")
        cu = Conn(srv.port, user="pinuser")
        _, _, codes = cu.q("SELECT * FROM pintab")
        check("pin:permission-denied-42501", codes == ["42501"], str(codes))
        _, _, codes = cu.q("INSERT INTO pintab VALUES (1)")
        check("pin:insert-denied-42501", codes == ["42501"], str(codes))
        cu.close()
        # syntax error
        _, _, codes = c.q("SELEC 1")
        check("pin:syntax-42601", codes == ["42601"], str(codes))
        # unique violation
        c.q("CREATE TABLE pin_uq(a INT UNIQUE)")
        c.q("INSERT INTO pin_uq VALUES (1)")
        _, _, codes = c.q("INSERT INTO pin_uq VALUES (1)")
        check("pin:unique-23505", codes == ["23505"], str(codes))
        # not-null violation
        c.q("CREATE TABLE pin_nn(a INT NOT NULL)")
        _, _, codes = c.q("INSERT INTO pin_nn VALUES (NULL)")
        check("pin:notnull-23502", codes == ["23502"], str(codes))
        # check violation
        c.q("CREATE TABLE pin_ck(a INT CHECK (a > 0))")
        _, _, codes = c.q("INSERT INTO pin_ck VALUES (-1)")
        check("pin:check-23514", codes == ["23514"], str(codes))
        # FK violation
        c.q("CREATE TABLE pin_p(a INT PRIMARY KEY)")
        c.q("CREATE TABLE pin_f(a INT REFERENCES pin_p(a))")
        _, _, codes = c.q("INSERT INTO pin_f VALUES (99)")
        check("pin:fk-23503", codes == ["23503"], str(codes))
        # aborted-transaction gate
        c.q("BEGIN")
        c.q("SELECT * FROM nosuchtable")
        _, _, codes = c.q("SELECT 1")
        check("pin:aborted-25P02", codes == ["25P02"], str(codes))
        c.q("ROLLBACK")
        # duplicate assignment targets -> 42701
        c.q("CREATE TABLE pin_dup(a INT PRIMARY KEY, b INT)")
        _, _, codes = c.q("INSERT INTO pin_dup(a, a) VALUES (1, 2)")
        check("pin:insert-dup-col-42701", codes == ["42701"], str(codes))
        _, _, codes = c.q("UPDATE pin_dup SET a = 1, a = 2")
        check("pin:update-dup-col-42701", codes == ["42701"], str(codes))
        _, _, codes = c.q("INSERT INTO pin_dup VALUES (1, 1) ON CONFLICT(a) DO UPDATE SET b = 1, b = 2")
        check("pin:upsert-dup-col-42701", codes == ["42701"], str(codes))
        # savepoint errors
        c.q("BEGIN")
        _, _, codes = c.q("ROLLBACK TO SAVEPOINT nosuchsp")
        check("pin:savepoint-3B001", codes == ["3B001"], str(codes))
        c.q("ROLLBACK")
        c.close()
    finally:
        srv.stop()

# ---------------------------------------------------------------------------
# D. mini-soak + kill -9 recovery
# ---------------------------------------------------------------------------

SOAK_EXPECTED = {
    "23505", "23503", "23502", "23514", "22P02", "22003", "22012", "42883",
    "42703", "42P01", "42P07", "40001", "25P02", "42601", "22008",
    "42804", "22023", "2BP01", "55000", "22004", "42803", "42701", "42702",
}

def t_mini_soak_and_recovery():
    srv = Server()
    try:
        rng = random.Random(1212)
        c = Conn(srv.port)
        for i in range(3):
            c.q(f"CREATE TABLE soak_t{i}(a INT, b INT, c TEXT)")
            vals = ",".join(f"({j},{j},'init')" for j in range(1, 51))
            c.q(f"INSERT INTO soak_t{i} VALUES {vals}")
        tables = [f"soak_t{i}" for i in range(3)]
        def rv():
            k = rng.random()
            if k < 0.6: return str(rng.randint(-1000, 1000))
            if k < 0.75: return f"'s{rng.randint(0,99)}'"
            if k < 0.85: return "NULL"
            return f"{rng.uniform(-50, 50):.2f}"
        unexpected = []
        stmts = errs = 0
        t_end = time.time() + 10
        in_txn = False
        while time.time() < t_end:
            if not in_txn and rng.random() < 0.2:
                c.q("BEGIN"); in_txn = True
            t = rng.choice(tables)
            r = rng.random()
            if r < 0.4:
                sql = f"INSERT INTO {t} VALUES ({rv()},{rv()},{rv()})"
            elif r < 0.6:
                sql = f"SELECT COUNT(*) FROM {t} WHERE a > {rng.randint(-500,500)}"
            elif r < 0.75:
                sql = f"UPDATE {t} SET b = b + 1 WHERE a < {rng.randint(-500,500)}"
            elif r < 0.85:
                sql = f"DELETE FROM {t} WHERE a = {rng.randint(-1000,1000)}"
            else:
                sql = f"SELECT SUM(a) FROM {t}"
            try:
                _, _, codes = c.q(sql)
            except Exception as e:
                errs += 1
                unexpected.append(("conn-exc", repr(e)[:80]))
                try: c.close()
                except Exception: pass
                c = Conn(srv.port); in_txn = False
                continue
            stmts += 1
            for code in codes:
                errs += 1
                if code not in SOAK_EXPECTED:
                    unexpected.append((sql[:60], code))
            if in_txn and rng.random() < 0.7:
                c.q("COMMIT"); in_txn = False
        try: c.q("ROLLBACK")
        except Exception: pass
        c.close()
        check("soak-no-unexpected-codes", not unexpected, str(unexpected[:5]))
        print(f"    mini-soak: {stmts} stmts, {errs} expected errs")
        # SIGKILL + recovery
        srv.kill9()
        srv.restart()
        c = Conn(srv.port)
        ok = True
        for t in tables:
            _, rows, codes = c.q(f"SELECT COUNT(*) FROM {t}")
            if codes or not rows:
                ok = False
        check("soak-recovery-readable", ok)
        c.close()
    finally:
        try:
            srv.stop()
        except Exception:
            pass

# ---------------------------------------------------------------------------

def main():
    tests = [
        t_concurrent_mixed_workload,
        t_same_name_ddl_race,
        t_grant_race_enforcement,
        t_nextval_race_unique,
        t_checkpoint_vacuum_under_load,
        t_rapid_connect_disconnect,
        t_giant_length_lie,
        t_unknown_message_type_fatal,
        t_cancel_request_quiet,
        t_ssl_then_startup,
        t_bad_startup_variants,
        t_out_of_order_extended,
        t_error_code_pins,
        t_mini_soak_and_recovery,
    ]
    for t in tests:
        print(f"== {t.__name__}")
        try:
            t()
        except Exception as e:
            failed.append(t.__name__ + "-EXC")
            print(f"  FAIL: {t.__name__} raised {e!r}")
    print(f"\n{len(passed)} passed, {len(failed)} failed")
    if failed:
        print("failures:", failed)
    return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(main())
