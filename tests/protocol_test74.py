#!/usr/bin/env python3
"""protocol_test74.py — v0.72 partition metadata durability.

Covers the v0.72 `PartitionInfo::is_partitioned` distinction (leaf vs.
childless partitioned intermediate) across a CHECKPOINT + restart:

1. A childless partitioned intermediate rejects direct inserts with
   23514 both before and after the restart (the flag survives the
   RGSCHK10 checkpoint roundtrip).
2. Routing still works after recovery: inserts through the parent land
   in the right leaf, and previously inserted rows are visible.
3. ATTACH preserves the leaf-vs-partitioned distinction: an attached
   childless partitioned table still rejects direct inserts after
   recovery.
4. The crash-regression shape (PARTITION OF + propagated ALTER)
   survives recovery: the tables are usable after restart.

This test manages its own server on port 5546 (so it never collides
with the conformance runner) and is RED if is_partitioned is dropped
by the checkpoint encode/decode path.
"""

import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5546
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = 0
failed = 0


def check(name, cond):
    global passed, failed
    if cond:
        passed += 1
        # print(f"PASS {name}")
    else:
        failed += 1
        print(f"FAIL {name}")


def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError("server did not start")


def stop_server(proc):
    try:
        proc.terminate()
        proc.wait(timeout=10)
    except Exception:
        proc.kill()
    time.sleep(1.0)


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            typ, _ = self._read_msg()
            if typ == b"Z":
                break

    def _read_exact(self, n):
        d = b""
        while len(d) < n:
            c = self.s.recv(n - len(d))
            if not c:
                raise RuntimeError("connection closed")
            d += c
        return d

    def _read_msg(self):
        typ = self._read_exact(1)
        ln = struct.unpack("!i", self._read_exact(4))[0]
        return typ, self._read_exact(ln - 4)

    def do_sql(self, q):
        qb = q.encode()
        self.s.sendall(struct.pack("!c", b"Q")
                       + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
        tag = None
        err = None
        rows = []
        nfields = 0
        while True:
            typ, payload = self._read_msg()
            if typ == b"T":
                nfields = struct.unpack("!H", payload[:2])[0]
            elif typ == b"D":
                pos = 2
                row = []
                for _ in range(nfields):
                    ln = struct.unpack("!i", payload[pos:pos + 4])[0]
                    pos += 4
                    if ln < 0:
                        row.append(None)
                    else:
                        row.append(payload[pos:pos + ln].decode())
                        pos += ln
                rows.append(tuple(row))
            elif typ == b"C":
                tag = payload[:-1].decode()
            elif typ == b"E":
                i = 0
                code = None
                while i < len(payload) - 1:
                    f = payload[i:i + 1]
                    end = payload.find(b"\x00", i + 1)
                    if f == b"C":
                        code = payload[i + 1:end].decode()
                    i = end + 1
                err = code
            elif typ == b"Z":
                break
        return err, tag, rows

    def close(self):
        self.s.close()


def main():
    data_dir = tempfile.mkdtemp(prefix="rg74_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # --- Setup: root -> childless intermediate (no children), -----
        # --- root -> intermediate -> leaf, plus an attached childless --
        # --- partitioned table. ---------------------------------------
        for stmt in [
            "create table p74_p (a int, b int) partition by range (a)",
            "create table p74_mid partition of p74_p "
            "for values from (1) to (100) partition by range (b)",
            "create table p74_mid2 partition of p74_p "
            "for values from (200) to (300) partition by range (b)",
            "create table p74_leaf partition of p74_mid2 "
            "for values from (1) to (50)",
            "create table p74_det (a int, b int) partition by range (b)",
            "alter table p74_p attach partition p74_det "
            "for values from (100) to (200)",
            "insert into p74_p values (250, 10)",
        ]:
            err, tag, _ = c.do_sql(stmt)
            check(f"setup: {stmt[:48]}", err is None)

        err, _, _ = c.do_sql("insert into p74_mid values (5, 5)")
        check("pre-checkpoint: direct insert into childless intermediate "
              "-> 23514", err == "23514")
        err, _, _ = c.do_sql("insert into p74_det values (150, 5)")
        check("pre-checkpoint: direct insert into attached childless "
              "partitioned table -> 23514", err == "23514")

        err, tag, _ = c.do_sql("CHECKPOINT")
        check("CHECKPOINT ok", err is None and tag == "CHECKPOINT")
        c.close()
        stop_server(proc)

        # --- Restart: everything must behave identically. --------------
        proc = start_server(data_dir)
        c = Conn()

        err, _, rows = c.do_sql("select count(*) from p74_p")
        check("recovered rows visible",
              err is None and rows == [("1",)])
        err, _, _ = c.do_sql("insert into p74_mid values (6, 6)")
        check("post-recovery: direct insert into childless intermediate "
              "-> 23514", err == "23514")
        err, _, _ = c.do_sql("insert into p74_det values (150, 6)")
        check("post-recovery: direct insert into attached childless "
              "partitioned table -> 23514", err == "23514")
        err, tag, _ = c.do_sql("insert into p74_p values (250, 20)")
        check("post-recovery: routing through parent still works",
              err is None and tag == "INSERT 0 1")
        err, _, rows = c.do_sql("select count(*) from p74_leaf")
        check("post-recovery: leaf holds both rows",
              err is None and rows == [("2",)])
        # The crash-regression shape stays usable after recovery.
        err, tag, _ = c.do_sql("alter table p74_p add d int")
        check("post-recovery: propagated ALTER works",
              err is None and tag == "ALTER TABLE")
        err, tag, _ = c.do_sql("insert into p74_leaf values (250, 8, 1)")
        check("post-recovery: leaf writable after ALTER",
              err is None and tag == "INSERT 0 1")
        c.close()
    finally:
        stop_server(proc)
        shutil.rmtree(data_dir, ignore_errors=True)

    print(f"protocol_test74: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
