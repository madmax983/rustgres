#!/usr/bin/env python3
"""protocol_test99.py — v1.05 TOAST UPDATE lifecycle (PG19 parity).

v1.05 closes the UPDATE lifecycle gaps the v1.04 audit found:
- UPDATE deletes superseded out-of-line TOAST values eagerly during the
  UPDATE itself (PG19 heap_update -> heap_toast_insert_or_update ->
  toast_tuple_cleanup), not only at commit-time vacuum: an explicit
  transaction sees 3 chunks immediately after UPDATE, not 6.
- UPDATE reuses the old value id for unchanged toasted columns
  (PG19 toast_tuple_init TOASTCOL_IGNORE): the chunk_id set is identical.
- Toasted -> inline/plain UPDATE deletes the old chunks.
- ROLLBACK of an UPDATE restores the old chunks and drops the new value's
  metadata (no orphan toast_info).
- INSERT .. ON CONFLICT DO UPDATE and partition-moving UPDATE get the same
  eager cleanup / reuse.
- The lazy reltoastrelid link (WriteOp::SetToastRelid) is WAL-logged
  (RGSWAL19) and rolls back.

This test manages its own server on port 5561 (so it can kill/restart
it) and is RED on the v1.04 base, GREEN on the v1.05 branch.

Sections:
  A. explicit-txn changed UPDATE: 3 chunks right after UPDATE (not 6)
  B. unchanged UPDATE reuses the same chunk_id (value id)
  C. toasted -> inline UPDATE deletes old chunks
  D. ROLLBACK of UPDATE restores old chunks, drops new metadata
  E. ON CONFLICT DO UPDATE: cleanup + reuse
  F. partition-moving UPDATE moves chunks with the row
  G. UPDATE chunks survive a restart (WAL-logged)
  H. repeated updates: no chunk/metadata accumulation
  I. WAL magic is RGSWAL19
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

PORT = 5561
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

random.seed(99)
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


def chunk_ids(c, table):
    tt = toast_table(c, table)
    rows, err, msg = c.do_sql(f"SELECT DISTINCT chunk_id FROM {tt}")
    assert not err, f"chunk ids -> {err}: {msg}"
    return sorted(r[0] for r in rows)


def main():
    data_dir = tempfile.mkdtemp(prefix="rg99_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # ---- A. explicit-txn changed UPDATE: eager cleanup (3, not 6) ----
        c.do_sql("DROP TABLE IF EXISTS t99u")
        c.do_sql("CREATE TABLE t99u(a int, b text)")
        c.do_sql(f"INSERT INTO t99u VALUES (1, '{BIGR}')")
        check("A1 pre-update chunks", chunk_count(c, "t99u") == 3)
        c.do_sql("BEGIN")
        c.do_sql(f"UPDATE t99u SET b = '{BIGR2}' WHERE a = 1")
        check("A2 chunks reaped eagerly inside txn (3, not 6)",
              chunk_count(c, "t99u") == 3)
        check("A3 new value visible inside txn",
              c.val("SELECT b FROM t99u WHERE a = 1") == BIGR2)
        c.do_sql("COMMIT")
        check("A4 chunks stable after commit", chunk_count(c, "t99u") == 3)
        check("A5 new value visible after commit",
              c.val("SELECT b FROM t99u WHERE a = 1") == BIGR2)

        # ---- B. unchanged UPDATE reuses the same value id ----
        before = chunk_ids(c, "t99u")
        check("B0 one distinct value id", len(before) == 1)
        c.do_sql("BEGIN")
        c.do_sql(f"UPDATE t99u SET b = '{BIGR2}' WHERE a = 1")
        check("B1 same chunk_id after unchanged UPDATE",
              chunk_ids(c, "t99u") == before)
        check("B2 still 3 chunks", chunk_count(c, "t99u") == 3)
        c.do_sql("COMMIT")
        check("B3 same chunk_id after commit", chunk_ids(c, "t99u") == before)

        # ---- C. toasted -> inline UPDATE deletes old chunks ----
        c.do_sql("UPDATE t99u SET b = 'tiny' WHERE a = 1")
        check("C1 old chunks deleted", chunk_count(c, "t99u") == 0)
        check("C2 inline value visible",
              c.val("SELECT b FROM t99u WHERE a = 1") == "tiny")

        # ---- D. ROLLBACK of UPDATE restores old chunks ----
        c.do_sql(f"UPDATE t99u SET b = '{BIGR}' WHERE a = 1")
        check("D0 back to 3 chunks", chunk_count(c, "t99u") == 3)
        before = chunk_ids(c, "t99u")
        c.do_sql("BEGIN")
        c.do_sql(f"UPDATE t99u SET b = '{BIGR2}' WHERE a = 1")
        check("D1 chunks reaped inside txn", chunk_count(c, "t99u") == 3)
        check("D2 new chunk_id inside txn", chunk_ids(c, "t99u") != before)
        c.do_sql("ROLLBACK")
        check("D3 rollback restores chunk count", chunk_count(c, "t99u") == 3)
        check("D4 rollback restores old chunk_id",
              chunk_ids(c, "t99u") == before)
        check("D5 rollback restores old value",
              c.val("SELECT b FROM t99u WHERE a = 1") == BIGR)
        check("D6 no extra chunks from aborted value",
              chunk_count(c, "t99u") == 3)

        # ---- E. ON CONFLICT DO UPDATE: cleanup + reuse ----
        c.do_sql("DROP TABLE IF EXISTS t99c")
        c.do_sql("CREATE TABLE t99c(a int PRIMARY KEY, b text)")
        c.do_sql(f"INSERT INTO t99c VALUES (1, '{BIGR}')")
        check("E1 pre-upsert chunks", chunk_count(c, "t99c") == 3)
        before = chunk_ids(c, "t99c")
        c.do_sql(f"INSERT INTO t99c VALUES (1, '{BIGR2}') "
                 "ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b")
        check("E2 upsert reaps old chunks (3, not 6)",
              chunk_count(c, "t99c") == 3)
        check("E3 upsert value visible",
              c.val("SELECT b FROM t99c WHERE a = 1") == BIGR2)
        after_upsert = chunk_ids(c, "t99c")
        # Unchanged upsert reuses the value id.
        c.do_sql(f"INSERT INTO t99c VALUES (1, '{BIGR2}') "
                 "ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b")
        check("E4 unchanged upsert reuses chunk_id",
              chunk_ids(c, "t99c") == after_upsert
              and len(after_upsert) == 1)

        # ---- F. partition-moving UPDATE moves chunks with the row ----
        c.do_sql("DROP TABLE IF EXISTS t99p")
        c.do_sql("CREATE TABLE t99p(a int, b text) PARTITION BY LIST (a)")
        c.do_sql("CREATE TABLE t99p1 PARTITION OF t99p FOR VALUES IN (1)")
        c.do_sql("CREATE TABLE t99p2 PARTITION OF t99p FOR VALUES IN (2)")
        c.do_sql(f"INSERT INTO t99p VALUES (1, '{BIGR}')")
        check("F1 source leaf has 3 chunks", chunk_count(c, "t99p1") == 3)
        c.do_sql("UPDATE t99p SET a = 2 WHERE a = 1")
        check("F2 source leaf chunks reaped", chunk_count(c, "t99p1") == 0)
        check("F3 dest leaf has 3 chunks", chunk_count(c, "t99p2") == 3)
        check("F4 moved value roundtrips",
              c.val("SELECT b FROM t99p WHERE a = 2") == BIGR)

        # ---- G. UPDATE chunks survive a restart (WAL-logged) ----
        c.do_sql(f"UPDATE t99u SET b = '{BIGR2}' WHERE a = 1")
        check("G1 pre-restart chunks", chunk_count(c, "t99u") == 3)
        c.close()
        stop_server(proc, sigkill=True)
        proc = start_server(data_dir)
        c = Conn()
        check("G2 post-restart chunks intact", chunk_count(c, "t99u") == 3)
        check("G3 post-restart value intact",
              c.val("SELECT b FROM t99u WHERE a = 1") == BIGR2)
        check("G4 post-restart compression metadata intact",
              c.val("SELECT pg_column_compression(b) FROM t99u WHERE a = 1")
              is None)

        # ---- H. repeated updates: no accumulation ----
        for i in range(5):
            v = BIGR if i % 2 == 0 else BIGR2
            c.do_sql(f"UPDATE t99u SET b = '{v}' WHERE a = 1")
        check("H1 still 3 chunks after 5 updates", chunk_count(c, "t99u") == 3)
        check("H2 one distinct value id", len(chunk_ids(c, "t99u")) == 1)
        check("H3 final value correct",
              c.val("SELECT b FROM t99u WHERE a = 1") == BIGR)
        c.close()
    finally:
        try:
            c.close()
        except Exception:
            pass
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    # ---- I. WAL magic (data dir already cleaned; use a fresh one) ----
    data_dir = tempfile.mkdtemp(prefix="rg99m_")
    proc = start_server(data_dir)
    try:
        c = Conn()
        c.do_sql("CREATE TABLE t99m(a int, b text)")
        c.close()
    finally:
        stop_server(proc)
    with open(os.path.join(data_dir, "wal.log"), "rb") as f:
        magic = f.read(8)
    check("I1 WAL magic is RGSWAL19", magic == b"RGSWAL19")
    shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol_test99: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
