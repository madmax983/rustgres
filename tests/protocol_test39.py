#!/usr/bin/env python3
"""protocol_test39.py — v0.39 TOAST durability and lifecycle.

v0.39 closes the TOAST durability/lifecycle gaps:
- WAL records now carry per-row toast flags and (value id, compressed)
  provenance, so TOAST metadata survives crash recovery via WAL replay
  (previously only checkpoints preserved it; WAL format RGSWAL08 -> RGSWAL09).
- DELETE removes a row version's out-of-line toast chunks immediately
  (PG19 heap_delete -> heap_toast_delete), staged as transactional
  WriteOps (WAL-logged; restored on ROLLBACK).
- VACUUM removes dead versions' toast chunks (they are unreachable).

This test manages its own server on port 5544 (so it can kill/restart
it) and is RED on the v0.38 base, GREEN on the v0.39 branch.

Sections:
  A. toast metadata survives WAL replay across a restart
  B. DELETE removes out-of-line chunks immediately
  C. UPDATE's old chunks are reaped by vacuum
  D. ROLLBACK of a DELETE restores chunks (regression guard)
  E. chunk deletions survive a restart (WAL-logged)
  F. WAL magic is RGSWAL17 (v0.41: method codes in toast_info;
     v0.72: is_partitioned in checkpoint/WAL records;
     v0.82: CreateType/DropType records;
     v0.85: WalDomain + domain metadata in CreateType/CreateTable/AlterTable;
     v0.88: CreateIndex direction/nulls/expr/predicate metadata;
     v0.96: inherits links in CreateTable/AlterTable;
     v0.98: sequence cache + owned_by in WalSequence)
"""
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5544
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = failed = 0


def check(name, cond):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")


# --------------------------------------------------------------------------
# Server lifecycle.
# --------------------------------------------------------------------------

def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # Wait for the port to accept.
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError("server did not start")


def stop_server(proc, sigkill=False):
    try:
        if sigkill:
            proc.kill()
        else:
            proc.terminate()
        proc.wait(timeout=10)
    except Exception:
        proc.kill()
    time.sleep(0.5)


# --------------------------------------------------------------------------
# Wire protocol.
# --------------------------------------------------------------------------

class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = self.read_msg()
            if t == b"Z":
                break

    def read_exact(self, n):
        d = b""
        while len(d) < n:
            c = self.s.recv(n - len(d))
            if not c:
                raise RuntimeError("closed")
            d += c
        return d

    def read_msg(self):
        t = self.read_exact(1)
        (ln,) = struct.unpack("!i", self.read_exact(4))
        return t, self.read_exact(ln - 4)

    def do_sql(self, q):
        self.s.sendall(b"Q" + struct.pack("!i", len(q.encode()) + 5)
                       + q.encode() + b"\x00")
        rows, err, msg = [], None, None
        while True:
            t, p = self.read_msg()
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, row = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos + 4])
                    pos += 4
                    if ln == -1:
                        row.append(None)
                    else:
                        row.append(p[pos:pos + ln].decode())
                        pos += ln
                rows.append(row)
            elif t == b"E":
                f, pos = {}, 0
                while pos < len(p) and p[pos] != 0:
                    c = chr(p[pos])
                    pos += 1
                    e = p.index(b"\x00", pos)
                    f[c] = p[pos:e].decode("utf8", "replace")
                    pos = e + 1
                err, msg = f.get("C"), f.get("M")
            elif t == b"Z":
                break
        return rows, err, msg

    def val(self, q):
        rows, err, msg = self.do_sql(q)
        assert not err, f"{q} -> SQLSTATE {err}: {msg}"
        assert len(rows) == 1 and len(rows[0]) == 1, f"{q} -> {rows}"
        return rows[0][0]

    def close(self):
        self.s.close()


# --------------------------------------------------------------------------
# Test data: 'x'*5000 compresses inline ('pglz', no chunks); high-entropy
# 5000 chars go out-of-line uncompressed (3 chunks, NULL compression).
# --------------------------------------------------------------------------

random.seed(39)
BIGC = "x" * 5000
BIGR = "".join(random.choice("abcdefghijklmnopqrstuvwxyz0123456789")
               for _ in range(5000))
BIGR2 = "".join(random.choice("ABCDEFGHIJKLMNOPQRSTUVWXYZ9876543210")
                for _ in range(5000))


def toast_table(c, table):
    return c.val(
        f"SELECT reltoastrelid::regclass FROM pg_class WHERE relname='{table}'")


def chunk_count(c, table):
    tt = toast_table(c, table)
    return int(c.val(f"SELECT count(*) FROM {tt}"))


def main():
    data_dir = tempfile.mkdtemp(prefix="rg39_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # ---- A. toast metadata survives WAL replay across a restart ----
        c.do_sql("DROP TABLE IF EXISTS t39w")
        c.do_sql("CREATE TABLE t39w(a int, b text)")
        c.do_sql(f"INSERT INTO t39w VALUES (1, '{BIGC}')")
        c.do_sql(f"INSERT INTO t39w VALUES (2, '{BIGR}')")
        check("A1 pre-restart pglz", c.val(
            "SELECT pg_column_compression(b) FROM t39w WHERE a = 1") == "pglz")
        check("A2 pre-restart chunks", chunk_count(c, "t39w") == 3)
        check("A3 pre-restart external NULL",
              c.val("SELECT pg_column_compression(b) FROM t39w WHERE a = 2")
              is None)
        c.close()
        stop_server(proc, sigkill=True)
        proc = start_server(data_dir)
        c = Conn()
        check("A4 post-restart pglz (WAL flags+provenance)",
              c.val("SELECT pg_column_compression(b) FROM t39w WHERE a = 1")
              == "pglz")
        check("A5 post-restart chunked value roundtrips",
              c.val("SELECT b FROM t39w WHERE a = 2") == BIGR)
        check("A6 post-restart chunks intact", chunk_count(c, "t39w") == 3)
        check("A7 post-restart external still NULL",
              c.val("SELECT pg_column_compression(b) FROM t39w WHERE a = 2")
              is None)
        # New toasted rows after a restart get fresh value ids (no reuse).
        c.do_sql(f"INSERT INTO t39w VALUES (3, '{BIGC}')")
        check("A8 post-restart insert pglz",
              c.val("SELECT pg_column_compression(b) FROM t39w WHERE a = 3")
              == "pglz")
        check("A9 old rows undisturbed",
              c.val("SELECT b FROM t39w WHERE a = 1") == BIGC
              and c.val("SELECT b FROM t39w WHERE a = 2") == BIGR)
        # UPDATE toast metadata also rides the WAL.
        c.do_sql(f"UPDATE t39w SET b = '{BIGC}' WHERE a = 2")
        c.close()
        stop_server(proc, sigkill=True)
        proc = start_server(data_dir)
        c = Conn()
        check("A10 post-restart updated pglz",
              c.val("SELECT pg_column_compression(b) FROM t39w WHERE a = 2")
              == "pglz")
        check("A11 post-restart updated value",
              c.val("SELECT b FROM t39w WHERE a = 2") == BIGC)

        # ---- B. DELETE removes out-of-line chunks immediately ----
        c.do_sql("DROP TABLE IF EXISTS t39d")
        c.do_sql("CREATE TABLE t39d(a int, b text)")
        c.do_sql(f"INSERT INTO t39d VALUES (1, '{BIGR}')")
        check("B1 pre-delete chunks", chunk_count(c, "t39d") == 3)
        c.do_sql("DELETE FROM t39d")
        check("B2 delete removes chunks", chunk_count(c, "t39d") == 0)
        check("B3 table empty", c.val("SELECT count(*) FROM t39d") == "0")

        # ---- C. UPDATE's old chunks are reaped by vacuum ----
        c.do_sql("DROP TABLE IF EXISTS t39u")
        c.do_sql("CREATE TABLE t39u(a int, b text)")
        c.do_sql(f"INSERT INTO t39u VALUES (1, '{BIGR}')")
        check("C1 pre-update chunks", chunk_count(c, "t39u") == 3)
        c.do_sql(f"UPDATE t39u SET b = '{BIGR2}' WHERE a = 1")
        check("C2 old chunks reaped (3, not 6)",
              chunk_count(c, "t39u") == 3)
        check("C3 new value visible",
              c.val("SELECT b FROM t39u WHERE a = 1") == BIGR2)

        # ---- D. ROLLBACK of a DELETE restores chunks ----
        c.do_sql("DROP TABLE IF EXISTS t39r")
        c.do_sql("CREATE TABLE t39r(a int, b text)")
        c.do_sql(f"INSERT INTO t39r VALUES (1, '{BIGR}')")
        check("D1 pre-txn chunks", chunk_count(c, "t39r") == 3)
        c.do_sql("BEGIN")
        c.do_sql("DELETE FROM t39r")
        check("D2 chunks gone inside txn", chunk_count(c, "t39r") == 0)
        c.do_sql("ROLLBACK")
        check("D3 rollback restores chunks", chunk_count(c, "t39r") == 3)
        check("D4 rollback restores value",
              c.val("SELECT b FROM t39r WHERE a = 1") == BIGR)

        # ---- E. chunk deletions survive a restart (WAL-logged) ----
        c.do_sql("DROP TABLE IF EXISTS t39e")
        c.do_sql("CREATE TABLE t39e(a int, b text)")
        c.do_sql(f"INSERT INTO t39e VALUES (1, '{BIGR}')")
        c.do_sql("DELETE FROM t39e")
        check("E1 pre-restart chunks gone", chunk_count(c, "t39e") == 0)
        c.close()
        stop_server(proc, sigkill=True)
        proc = start_server(data_dir)
        c = Conn()
        check("E2 post-restart chunks still gone",
              chunk_count(c, "t39e") == 0)
        check("E3 post-restart table empty",
              c.val("SELECT count(*) FROM t39e") == "0")
        c.close()
    finally:
        try:
            c.close()
        except Exception:
            pass
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    # ---- F. WAL magic (data dir already cleaned; use a fresh one) ----
    data_dir = tempfile.mkdtemp(prefix="rg39m_")
    proc = start_server(data_dir)
    try:
        c = Conn()
        c.do_sql("CREATE TABLE t39m(a int)")
        c.close()
    finally:
        stop_server(proc)
    with open(os.path.join(data_dir, "wal.log"), "rb") as f:
        magic = f.read(8)
    check("F1 WAL magic is RGSWAL17", magic == b"RGSWAL17")
    shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol_test39: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
