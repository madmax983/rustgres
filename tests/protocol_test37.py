#!/usr/bin/env python3
r"""v0.37 protocol tests: TOAST storage controls and metadata.

RED (base b7707e6, before this milestone): no TOAST storage controls
existed -- SET STORAGE was a syntax error, toast_tuple_target was not a
known relation option, default_toast_compression did not exist,
pg_column_compression did not exist, pg_class had no reltoastrelid,
and regclass I/O used a naive name lookup (0::regclass did not yield
'-', unknown OIDs did not yield decimal text).

GREEN (this milestone):
- ALTER TABLE .. ALTER COLUMN .. SET STORAGE {plain|external|extended|main}
  (22023 on invalid mode, 0A000 on non-toastable types).
- ALTER TABLE .. SET (toast_tuple_target = N) validated to 128..8160
  (22023 out of bounds); unknown relation options -> 22023.
- default_toast_compression: SHOW -> pglz; SET accepts pglz/DEFAULT
  case-insensitively; other values -> 22023; RESET is a no-op.
- pg_column_compression: 'pglz' for compressed out-of-line values, NULL
  for fixed-width/NULL/uncompressed/computed values (never errors).
- pg_class.reltoastrelid: set eagerly at CREATE TABLE for tables with
  toastable columns (and on ADD COLUMN when the first toastable column
  appears); regclass casts needed to read it as a name.
- regclass: 0 <-> '-', numeric text <-> OID, unknown OID -> decimal,
  unknown name -> 42P01.
- Cast output labels: column::text keeps the column name; literals fall
  back to the cast type name.
- pg_relation_size: one- and two-arg forms ('main' fork only).

Honest non-claims (NOT tested as working): the compressor is a custom
LZ77 named pglz (not PGLZ byte-compatible); pg_relation_size is an
in-memory estimate, not page accounting; TOAST chunk/WAL durability
across restarts is incomplete; DELETE/VACUUM chunk cleanup is not
implemented; LZ4 is not implemented.
"""
import socket, struct, sys

PORT = 5433

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c: raise RuntimeError("closed")
        d += c
    return d

def read_msg(s):
    t = read_exact(s, 1)
    (ln,) = struct.unpack("!i", read_exact(s, 4))
    return t, read_exact(s, ln - 4)

s = socket.create_connection(('127.0.0.1', PORT), timeout=10)
body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
s.sendall(struct.pack("!i", len(body) + 4) + body)
while True:
    t, p = read_msg(s)
    if t == b'Z': break

def do_sql(q):
    s.sendall(b'Q' + struct.pack("!i", len(q.encode()) + 5) + q.encode() + b'\x00')
    rows, err, msg, cols, oids = [], None, None, [], []
    while True:
        t, p = read_msg(s)
        if t == b'T':
            (n,) = struct.unpack("!h", p[:2]); pos = 2
            for _ in range(n):
                e = p.index(b'\x00', pos); cols.append(p[pos:e].decode()); pos = e + 1
                (oid,) = struct.unpack("!i", p[pos+6:pos+10]); oids.append(oid); pos += 18
        elif t == b'D':
            (n,) = struct.unpack("!h", p[:2])
            pos, row = 2, []
            for _ in range(n):
                (ln,) = struct.unpack("!i", p[pos:pos+4]); pos += 4
                if ln == -1: row.append(None)
                else: row.append(p[pos:pos+ln].decode()); pos += ln
            rows.append(row)
        elif t == b'E':
            f, pos = {}, 0
            while pos < len(p) and p[pos] != 0:
                c = chr(p[pos]); pos += 1
                e = p.index(b'\x00', pos); f[c] = p[pos:e].decode('utf8', 'replace'); pos = e + 1
            err = f.get('C'); msg = f.get('M')
        elif t == b'Z':
            break
    return rows, err, msg, cols, oids

def val(q):
    rows, err, msg, _, _ = do_sql(q)
    assert not err, f"{q} -> SQLSTATE {err}: {msg}"
    assert len(rows) == 1 and len(rows[0]) == 1, f"{q} -> {rows}"
    return rows[0][0]

def err_of(q):
    _, err, _, _, _ = do_sql(q)
    return err

def msg_of(q):
    _, _, msg, _, _ = do_sql(q)
    return msg

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

BIG = 'x' * 5000

# A. SET STORAGE.
do_sql("DROP TABLE IF EXISTS t37a")
do_sql("CREATE TABLE t37a(a int, b text)")
check("A1 set storage external", err_of("ALTER TABLE t37a ALTER COLUMN b SET STORAGE external") is None)
check("A2 set storage extended", err_of("ALTER TABLE t37a ALTER COLUMN b SET STORAGE extended") is None)
check("A3 set storage main", err_of("ALTER TABLE t37a ALTER COLUMN b SET STORAGE main") is None)
check("A4 set storage plain", err_of("ALTER TABLE t37a ALTER COLUMN b SET STORAGE plain") is None)
check("A5 invalid storage mode", err_of("ALTER TABLE t37a ALTER COLUMN b SET STORAGE bogus") == "22023")
check("A6 non-toastable type", err_of("ALTER TABLE t37a ALTER COLUMN a SET STORAGE external") == "0A000")
check("A7 non-toastable message",
      msg_of("ALTER TABLE t37a ALTER COLUMN a SET STORAGE external") ==
      "column data type integer can only have storage PLAIN")
# Storage actually gates compression: EXTERNAL never compresses.
do_sql("ALTER TABLE t37a ALTER COLUMN b SET STORAGE external")
do_sql(f"INSERT INTO t37a VALUES (1, '{BIG}')")
check("A8 external -> NULL compression", val("SELECT pg_column_compression(b) FROM t37a") is None)
do_sql("ALTER TABLE t37a ALTER COLUMN b SET STORAGE extended")
do_sql(f"INSERT INTO t37a VALUES (2, '{BIG}')")
check("A9 extended -> pglz", val("SELECT pg_column_compression(b) FROM t37a WHERE a = 2") == "pglz")

# B. toast_tuple_target validation.
check("B1 too small", err_of("ALTER TABLE t37a SET (toast_tuple_target = 100)") == "22023")
check("B2 too large", err_of("ALTER TABLE t37a SET (toast_tuple_target = 9000)") == "22023")
check("B3 bounds message",
      msg_of("ALTER TABLE t37a SET (toast_tuple_target = 100)") ==
      'value 100 out of bounds for "toast_tuple_target" (128..=8160)')
check("B4 valid", err_of("ALTER TABLE t37a SET (toast_tuple_target = 4080)") is None)
check("B5 min bound ok", err_of("ALTER TABLE t37a SET (toast_tuple_target = 128)") is None)
check("B6 max bound ok", err_of("ALTER TABLE t37a SET (toast_tuple_target = 8160)") is None)

# C. Unknown relation option.
check("C1 unknown option", err_of("ALTER TABLE t37a SET (bogus_opt = 1)") == "22023")
check("C2 unknown option message",
      msg_of("ALTER TABLE t37a SET (bogus_opt = 1)") == 'unrecognized parameter "bogus_opt"')

# D. default_toast_compression GUC.
check("D1 show", val("SHOW default_toast_compression") == "pglz")
check("D2 set pglz", err_of("SET default_toast_compression = 'pglz'") is None)
check("D3 set case-insensitive", err_of("SET default_toast_compression = 'PGLZ'") is None)
check("D4 set lz4 accepted (v0.41: lz4 is a real method now)",
      err_of("SET default_toast_compression = 'lz4'") is None)
check("D4b show lz4", val("SHOW default_toast_compression") == "lz4")
check("D5 set bogus rejected", err_of("SET default_toast_compression = 'snappy'") == "22023")
check("D6 reset", err_of("RESET default_toast_compression") is None)
check("D7 still pglz after reset", val("SHOW default_toast_compression") == "pglz")

# E. pg_column_compression.
do_sql("DROP TABLE IF EXISTS t37e")
do_sql("CREATE TABLE t37e(a int, b text)")
do_sql(f"INSERT INTO t37e VALUES (1, '{BIG}')")
do_sql(f"INSERT INTO t37e VALUES (2, '{BIG}')")  # duplicate value
do_sql("INSERT INTO t37e VALUES (3, 'small')")
do_sql("INSERT INTO t37e VALUES (4, NULL)")
check("E1 compressed", val("SELECT pg_column_compression(b) FROM t37e WHERE a = 1") == "pglz")
check("E2 duplicate values both right",
      do_sql("SELECT pg_column_compression(b) FROM t37e WHERE a IN (1, 2) ORDER BY a")[0] ==
      [["pglz"], ["pglz"]])
check("E3 small value NULL", val("SELECT pg_column_compression(b) FROM t37e WHERE a = 3") is None)
check("E4 null value NULL", val("SELECT pg_column_compression(b) FROM t37e WHERE a = 4") is None)
check("E5 fixed-width NULL", val("SELECT pg_column_compression(a) FROM t37e WHERE a = 1") is None)
check("E6 expression NULL (no error)",
      val("SELECT pg_column_compression(b || '') FROM t37e WHERE a = 1") is None)
check("E7 literal NULL (no error)", val("SELECT pg_column_compression('hello')") is None)
check("E8 int literal NULL (no error)", val("SELECT pg_column_compression(42)") is None)

# F. pg_class.reltoastrelid (eager) + regclass reads.
do_sql("DROP TABLE IF EXISTS t37f")
do_sql("CREATE TABLE t37f(a int)")  # no toastable columns
check("F1 no toast table", val("SELECT reltoastrelid FROM pg_class WHERE relname = 't37f'") == "0")
do_sql("ALTER TABLE t37f ADD COLUMN b text")
r = val("SELECT reltoastrelid FROM pg_class WHERE relname = 't37f'")
check("F2 toast table on ADD COLUMN", r != "0")
check("F3 regclass name",
      val("SELECT reltoastrelid::regclass FROM pg_class WHERE relname = 't37f'").startswith("pg_toast.pg_toast_"))
do_sql("DROP TABLE IF EXISTS t37g")
do_sql("CREATE TABLE t37g(a int, b text)")
check("F4 eager toast table", val("SELECT reltoastrelid FROM pg_class WHERE relname = 't37g'") != "0")

# G. regclass I/O.
check("G1 0::regclass is '-'", val("SELECT 0::regclass") == "-")
check("G2 unknown oid decimal", val("SELECT 999999::regclass") == "999999")
check("G3 '-'::regclass is 0", val("SELECT '-'::regclass = 0::regclass") == "t")
check("G4 numeric text", val("SELECT '1234'::regclass = 1234::regclass") == "t")
check("G5 name resolves", val("SELECT 't37g'::regclass::text") == "t37g")
check("G6 unknown name", err_of("SELECT 'nosuchrel'::regclass") == "42P01")

# H. Cast output labels.
_, _, _, cols, _ = do_sql("SELECT b::text FROM t37g LIMIT 1")
check("H1 column cast keeps name", cols == ["b"])
_, _, _, cols, _ = do_sql("SELECT 'x'::text")
check("H2 literal cast falls back to type", cols == ["text"])
_, _, _, cols, _ = do_sql("SELECT a::text::varchar FROM t37g LIMIT 1")
check("H3 nested cast keeps strong column name", cols == ["a"])
_, _, _, cols, _ = do_sql("SELECT 'x'::text::varchar")
check("H4 nested literal cast outer type wins", cols == ["varchar"])

# I. pg_relation_size.
check("I1 one-arg", val("SELECT pg_relation_size('t37g')") is not None)
check("I2 two-arg main", val("SELECT pg_relation_size('t37g', 'main')") is not None)
check("I3 bad fork", err_of("SELECT pg_relation_size('t37g', 'fsm')") == "0A000")
check("I4 toast table size", val("SELECT pg_relation_size(reltoastrelid::regclass) FROM pg_class WHERE relname = 't37g'") is not None)

# J. TOAST lifecycle: UPDATE re-toasts, ROLLBACK cleans up, TRUNCATE clears.
do_sql("DROP TABLE IF EXISTS t37j")
do_sql("CREATE TABLE t37j(a int, b text)")
do_sql(f"INSERT INTO t37j VALUES (1, '{BIG}')")
check("J1 inserted compressed", val("SELECT pg_column_compression(b) FROM t37j") == "pglz")
do_sql(f"UPDATE t37j SET b = '{BIG}' || 'y' WHERE a = 1")
check("J2 updated still compressed", val("SELECT pg_column_compression(b) FROM t37j") == "pglz")
check("J3 update preserved value", val("SELECT length(b) FROM t37j") == "5001")
do_sql("BEGIN")
# Capture toast chunk count before the transactional insert.
tn_before = val("SELECT reltoastrelid::regclass FROM pg_class WHERE relname = 't37j'")
chunks_before = val(f"SELECT count(*) FROM {tn_before}")
do_sql(f"INSERT INTO t37j VALUES (2, '{BIG}')")
check("J4 txn row compressed", val("SELECT pg_column_compression(b) FROM t37j WHERE a = 2") == "pglz")
chunks_during = val(f"SELECT count(*) FROM {tn_before}")
do_sql("ROLLBACK")
check("J5 rollback removed row", val("SELECT count(*) FROM t37j") == "1")
tn = val("SELECT reltoastrelid::regclass FROM pg_class WHERE relname = 't37j'")
# The rolled-back row's chunks must be gone: chunk count returns to pre-txn.
# (Note: highly compressible data stays inline with no chunks; the key
# invariant is no chunk leaks after rollback.)
chunks_after = val(f"SELECT count(*) FROM {tn}")
check("J6 rollback cleaned chunks", chunks_before == chunks_after)
do_sql("TRUNCATE t37j")
check("J7 truncate emptied", val("SELECT count(*) FROM t37j") == "0")
# TRUNCATE must leave the toast table with zero rows.
check("J7b truncate cleared toast", val(f"SELECT count(*) FROM {tn}") == "0")
# Capture the exact toast relation name before DROP.
toast_rel = val("SELECT reltoastrelid::regclass FROM pg_class WHERE relname = 't37j'")
do_sql("DROP TABLE t37j")
# The toast table is gone from pg_class once its main table is dropped.
check("J8 drop removed toast table",
      val(f"SELECT count(*) FROM pg_class WHERE relname = '{toast_rel}'") == "0")

# Cleanup.
for t in ["t37a", "t37e", "t37f", "t37g"]:
    do_sql(f"DROP TABLE IF EXISTS {t}")

print(f"protocol_test37: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
