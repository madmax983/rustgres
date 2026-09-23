#!/usr/bin/env python3
"""protocol_test41.py — v0.41 PG19 LZ4 TOAST compression.

v0.41 adds a second TOAST compressor: PG19's LZ4 path from
`src/backend/access/common/toast_compression.c` (REL_19_STABLE), with
`lz4_compress_datum()` refusal semantics (reject only when the block is
*larger* than the input; the shared >2-byte net-win rule stays in the
planner). The pure-std Rust encoder emits structurally valid LZ4 block
streams — the genuine vendored `lz4.c` `LZ4_decompress_safe()` decodes
them (verified by `hidden_files/lz4ref/cdecode.c`; 9/9 compressible
cases), and the Rust decoder accepts real `LZ4_compress_default()`
output (`tests/data/lz4_vectors.txt`, 11 vectors).

Also bundled, following PG19's `GetAttributeCompression` /
`ATExecSetCompression`:
- `default_toast_compression` is a real enum GUC (pglz/lz4), defaulting
  to pglz like PG19 (even LZ4-enabled PG builds default to pglz);
  SHOW/SET/RESET/RESET ALL work, unknown values are 22023.
- Column-definition `COMPRESSION pglz|lz4|DEFAULT`, and
  `ALTER TABLE .. ALTER COLUMN .. SET COMPRESSION ..`.
  Explicit non-default compression on a non-toastable type is 0A000;
  unknown methods are 22023; SET COMPRESSION only affects future writes.
- `pg_column_compression()` reports the actual method ('pglz'/'lz4').
- Compression methods survive WAL replay and checkpoints
  (RGSWAL11/RGSCHK09 as of v0.72).

This test manages its own server on port 5545 (so it never collides
with the conformance runner) and is RED on the v0.40 base, GREEN on
the v0.41 branch.

Sections:
  A. GUC: default is lz4; SET/SHOW/RESET/RESET ALL; 22023 on bogus.
  B. CREATE TABLE .. COMPRESSION: per-column methods proven by
     pg_column_compression; 0A000/22023 validation.
  C. ALTER .. SET COMPRESSION: metadata for future writes only;
     DEFAULT reverts; 42703/0A000/22023 validation.
  D. LZ4 chunk structure: 4-byte LE original-length frame on chunk 0;
     round-trip; session default steers new writes.
  E. Durability: CHECKPOINT + restart keeps methods, settings, values.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5545
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


def toast_table(c, table):
    return c.val(
        f"SELECT reltoastrelid::regclass FROM pg_class WHERE relname='{table}'")


def main():
    data_dir = tempfile.mkdtemp(prefix="rg41_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # A. GUC: PG19 default is pglz (even in LZ4 builds);
        # SET/SHOW/RESET/RESET ALL.
        check("A1 default is pglz",
              c.val("SHOW default_toast_compression") == "pglz")
        c.do_sql("SET default_toast_compression = 'lz4'")
        check("A2 SET lz4",
              c.val("SHOW default_toast_compression") == "lz4")
        _, err, _ = c.do_sql("SET default_toast_compression = 'bogus'")
        check("A3 bogus -> 22023", err == "22023")
        c.do_sql("RESET default_toast_compression")
        check("A4 RESET restores pglz",
              c.val("SHOW default_toast_compression") == "pglz")
        c.do_sql("SET default_toast_compression = 'lz4'")
        c.do_sql("RESET ALL")
        check("A5 RESET ALL restores pglz",
              c.val("SHOW default_toast_compression") == "pglz")

        # B. Column COMPRESSION at CREATE; methods proven per column.
        c.do_sql("DROP TABLE IF EXISTS t41c")
        c.do_sql("CREATE TABLE t41c(a text COMPRESSION lz4, "
                 "b text COMPRESSION pglz, c text)")
        c.do_sql("ALTER TABLE t41c SET (toast_tuple_target = 128)")
        c.do_sql("INSERT INTO t41c VALUES "
                 "(repeat('L', 3000), repeat('P', 3000), repeat('D', 3000))")
        check("B1 explicit lz4",
              c.val("SELECT pg_column_compression(a) FROM t41c") == "lz4")
        check("B2 explicit pglz",
              c.val("SELECT pg_column_compression(b) FROM t41c") == "pglz")
        check("B3 default (session pglz)",
              c.val("SELECT pg_column_compression(c) FROM t41c") == "pglz")
        check("B4 roundtrip",
              c.val("SELECT length(a) || '/' || length(b) || '/' || length(c)"
                    " FROM t41c") == "3000/3000/3000")
        _, err, _ = c.do_sql(
            "CREATE TABLE t41bad1(x int COMPRESSION lz4)")
        check("B5 int COMPRESSION lz4 -> 0A000", err == "0A000")
        _, err, _ = c.do_sql(
            "CREATE TABLE t41bad2(x text COMPRESSION snappy)")
        check("B6 unknown method -> 22023", err == "22023")
        c.do_sql("CREATE TABLE t41dflt(x text COMPRESSION DEFAULT)")
        check("B7 COMPRESSION DEFAULT accepted",
              c.val("SELECT count(*) FROM t41dflt") == "0")

        # C. ALTER .. SET COMPRESSION: future writes only.
        c.do_sql("DROP TABLE IF EXISTS t41a")
        c.do_sql("CREATE TABLE t41a(v text)")
        c.do_sql("ALTER TABLE t41a SET (toast_tuple_target = 128)")
        c.do_sql("INSERT INTO t41a VALUES (repeat('O', 3000))")
        check("C1 old row is pglz",
              c.val("SELECT pg_column_compression(v) FROM t41a") == "pglz")
        c.do_sql("ALTER TABLE t41a ALTER COLUMN v SET COMPRESSION lz4")
        check("C2 existing value NOT rewritten",
              c.val("SELECT pg_column_compression(v) FROM t41a") == "pglz")
        c.do_sql("INSERT INTO t41a VALUES (repeat('N', 3000))")
        check("C3 new row uses lz4",
              c.val("SELECT pg_column_compression(v) FROM t41a "
                    "WHERE v LIKE 'N%'") == "lz4")
        check("C4 old row still pglz",
              c.val("SELECT pg_column_compression(v) FROM t41a "
                    "WHERE v LIKE 'O%'") == "pglz")
        c.do_sql("ALTER TABLE t41a ALTER COLUMN v SET COMPRESSION DEFAULT")
        c.do_sql("INSERT INTO t41a VALUES (repeat('M', 3000))")
        check("C5 DEFAULT reverts to session pglz",
              c.val("SELECT pg_column_compression(v) FROM t41a "
                    "WHERE v LIKE 'M%'") == "pglz")
        _, err, _ = c.do_sql(
            "ALTER TABLE t41a ALTER COLUMN nosuch SET COMPRESSION pglz")
        check("C6 missing column -> 42703", err == "42703")
        c.do_sql("ALTER TABLE t41a ADD COLUMN i int")
        _, err, _ = c.do_sql(
            "ALTER TABLE t41a ALTER COLUMN i SET COMPRESSION lz4")
        check("C7 int SET COMPRESSION -> 0A000", err == "0A000")
        _, err, _ = c.do_sql(
            "ALTER TABLE t41a ALTER COLUMN v SET COMPRESSION snappy")
        check("C8 unknown method -> 22023", err == "22023")
        # ADD COLUMN with COMPRESSION.
        c.do_sql("ALTER TABLE t41a ADD COLUMN w text COMPRESSION pglz")
        c.do_sql("INSERT INTO t41a(v, w) VALUES "
                 "(repeat('V', 3000), repeat('W', 3000))")
        check("C9 ADD COLUMN .. COMPRESSION pglz",
              c.val("SELECT pg_column_compression(w) FROM t41a "
                    "WHERE w LIKE 'W%'") == "pglz")

        # D. LZ4 chunk structure + session default steering new writes.
        # (19980 bytes of text: LZ4-compresses to 120 bytes, still over
        # the 128 target, so it goes out-of-line compressed.)
        c.do_sql("DROP TABLE IF EXISTS t41lz")
        # D. LZ4 chunk structure + session default steering new writes.
        # (19980 bytes of text: LZ4-compresses to 120 bytes, still over
        # the 128 target, so it goes out-of-line compressed. The column
        # uses explicit COMPRESSION lz4 since the PG19 session default
        # is pglz.)
        c.do_sql("DROP TABLE IF EXISTS t41lz")
        c.do_sql("CREATE TABLE t41lz(a text COMPRESSION lz4)")
        c.do_sql("ALTER TABLE t41lz SET (toast_tuple_target = 128)")
        c.do_sql("INSERT INTO t41lz VALUES "
                 "(repeat('The quick brown fox jumps. ', 740))")
        tt = toast_table(c, "t41lz")
        chunk0 = c.val(f"SELECT encode(chunk_data, 'hex') FROM {tt} "
                       f"WHERE chunk_seq = 0")
        # Framed form: 4-byte LE original length of the full value, then
        # the raw LZ4 block. 19980 = 0x4E0C.
        check("D1 chunk0 starts with LE length frame",
              chunk0[:8] == "0c4e0000")
        check("D2 chunk smaller than original",
              len(chunk0) // 2 < 19980)
        check("D3 method is lz4",
              c.val("SELECT pg_column_compression(a) FROM t41lz") == "lz4")
        check("D4 roundtrip",
              c.val("SELECT length(a) FROM t41lz") == "19980")
        # Session default steers new writes (PG semantics).
        c.do_sql("DROP TABLE IF EXISTS t41d")
        c.do_sql("CREATE TABLE t41d(a text)")
        c.do_sql("ALTER TABLE t41d SET (toast_tuple_target = 128)")
        c.do_sql("SET default_toast_compression = 'lz4'")
        c.do_sql("INSERT INTO t41d VALUES (repeat('cd', 1500))")
        check("D5 SET lz4 steers new write",
              c.val("SELECT pg_column_compression(a) FROM t41d") == "lz4")
        c.do_sql("RESET default_toast_compression")
        c.do_sql("INSERT INTO t41d VALUES (repeat('ef', 1500))")
        check("D6 RESET restores pglz default",
              c.val("SELECT pg_column_compression(a) FROM t41d "
                    "WHERE a LIKE 'ef%'") == "pglz")

        # E. Durability: CHECKPOINT + restart keeps methods and settings.
        c.do_sql("CHECKPOINT")
        c.close()
        stop_server(proc)
        proc = start_server(data_dir)
        c = Conn()
        check("E1 t41c column methods survive restart",
              c.val("SELECT pg_column_compression(a) FROM t41c") == "lz4"
              and c.val("SELECT pg_column_compression(b) FROM t41c") == "pglz")
        check("E2 SET COMPRESSION lz4 survives restart",
              c.val("SELECT pg_column_compression(v) FROM t41a "
                    "WHERE v LIKE 'N%'") == "lz4")
        check("E3 values intact after restart",
              c.val("SELECT length(a) FROM t41lz WHERE a LIKE 'The quick%'")
              == "19980"
              and c.val("SELECT length(v) FROM t41a WHERE v LIKE 'O%'") == "3000")
        check("E4 GUC back to default after restart",
              c.val("SHOW default_toast_compression") == "pglz")

        c.do_sql("DROP TABLE t41c")
        c.do_sql("DROP TABLE t41a")
        c.do_sql("DROP TABLE t41lz")
        c.do_sql("DROP TABLE t41d")
        c.do_sql("DROP TABLE t41dflt")
        c.close()
    finally:
        stop_server(proc)

    print(f"protocol_test41: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
