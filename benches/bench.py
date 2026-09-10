#!/usr/bin/env python3
"""Wire-protocol benchmarks for rustgres.

Drives the server over TCP using the real Postgres 3.0 wire protocol
(handshake + simple query + extended query), measures queries/sec and
latency percentiles. Pure stdlib python3 only.

Usage:
    # terminal 1: start the server
    cargo run
    # terminal 2:
    python3 benches/bench.py --seconds 5
    python3 benches/bench.py --workload scan --seconds 10
    python3 benches/bench.py --workload prepared --seconds 5
"""
import argparse
import socket
import struct
import sys
import threading
import time

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433


# ---------------------------------------------------------------------------
# Wire framing (mirrors tests/protocol_test.py, plus extended-protocol msgs)
#
# NOTE: reads go through a 64 KB userspace buffer, not one recv() per
# field. The naive pattern (recv(1) for the type byte, recv(4) for the
# length, ...) costs ~3 syscalls per message and made the 10k-row scan
# workload 4.5x slower than the server really is (measured 2026-09-10).
# ---------------------------------------------------------------------------

class Reader:
    """Buffered message reader over a socket."""

    def __init__(self, sock):
        self.sock = sock
        self.buf = bytearray()

    def _take(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("connection closed by server")
            self.buf.extend(chunk)
        out = bytes(self.buf[:n])
        del self.buf[:n]
        return out

    def read_msg(self):
        hdr = self._take(5)
        (ln,) = struct.unpack("!i", hdr[1:5])
        assert ln >= 4, f"bad message length {ln}"
        return hdr[0:1], self._take(ln - 4)

    def read_until(self, want):
        msgs = []
        while True:
            t, p = self.read_msg()
            msgs.append((t, p))
            if t == want:
                return msgs


def cstr(s):
    return s.encode() + b"\x00"


def msg(typ, body):
    return typ + struct.pack("!i", len(body) + 4) + body


def parse_cstring(payload, pos):
    end = payload.index(b"\x00", pos)
    return payload[pos:end].decode(), end + 1


def parse_datarow(payload):
    (ncols,) = struct.unpack("!h", payload[:2])
    pos = 2
    vals = []
    for _ in range(ncols):
        (ln,) = struct.unpack("!i", payload[pos:pos + 4])
        pos += 4
        if ln == -1:
            vals.append(None)
        else:
            vals.append(payload[pos:pos + ln].decode())
            pos += ln
    return vals


class Conn:
    """One Postgres-protocol connection."""

    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=30)
        self.rd = Reader(self.s)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        body = struct.pack("!i", 196608) + params
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self.rd.read_until(b"Z")  # handshake

    def simple(self, sql):
        """Simple-query protocol: returns list of (type, payload)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        return self.rd.read_until(b"Z")

    def ext(self, parts):
        """Send concatenated extended-protocol messages + Sync, read to 'Z'."""
        self.s.sendall(b"".join(parts) + msg(b"S", b""))
        return self.rd.read_until(b"Z")

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        finally:
            self.s.close()


# Extended-protocol message builders
def m_parse(stmt, query, param_oids=()):
    body = cstr(stmt) + cstr(query) + struct.pack("!h", len(param_oids))
    for oid in param_oids:
        body += struct.pack("!i", oid)
    return msg(b"P", body)


def m_bind(portal, stmt, param_values=(), result_formats=(0,)):
    body = cstr(portal) + cstr(stmt)
    body += struct.pack("!h", 1) + struct.pack("!h", 0)  # text param formats
    body += struct.pack("!h", len(param_values))
    for v in param_values:
        if v is None:
            body += struct.pack("!i", -1)
        else:
            b = v.encode()
            body += struct.pack("!i", len(b)) + b
    body += struct.pack("!h", len(result_formats))
    for f in result_formats:
        body += struct.pack("!h", f)
    return msg(b"B", body)


def m_describe(kind, name):
    return msg(b"D", kind + cstr(name))


def m_execute(portal, max_rows=0):
    return msg(b"E", cstr(portal) + struct.pack("!i", max_rows))


def m_close(kind, name):
    return msg(b"C", kind + cstr(name))


# ---------------------------------------------------------------------------
# Measurement helpers
# ---------------------------------------------------------------------------

def percentile(sorted_vals, p):
    if not sorted_vals:
        return 0.0
    k = (len(sorted_vals) - 1) * (p / 100.0)
    f = int(k)
    c = min(f + 1, len(sorted_vals) - 1)
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def measure(fn, seconds, warmup=1.0):
    """Run fn() repeatedly: warmup, then timed measurement.

    fn() performs exactly one measured operation. Returns a result dict.
    """
    # warmup
    end = time.perf_counter() + warmup
    while time.perf_counter() < end:
        fn()
    # measured
    lat = []
    end = time.perf_counter() + seconds
    while time.perf_counter() < end:
        t0 = time.perf_counter()
        fn()
        lat.append((time.perf_counter() - t0) * 1000.0)  # ms
    lat.sort()
    total_s = sum(lat) / 1000.0
    return {
        "ops": len(lat),
        "qps": len(lat) / total_s if total_s > 0 else 0.0,
        "p50_ms": percentile(lat, 50),
        "p99_ms": percentile(lat, 99),
    }


def tag_of(msgs):
    for t, p in msgs:
        if t == b"C":
            tag, _ = parse_cstring(p, 0)
            return tag
    return None


def has_error(msgs):
    return any(t == b"E" for t, _ in msgs)


# ---------------------------------------------------------------------------
# Workloads
# ---------------------------------------------------------------------------

def w_select1(conn, seconds):
    def op():
        msgs = conn.simple("SELECT 1")
        assert tag_of(msgs) == "SELECT 1", "unexpected response"
    return measure(op, seconds)


def w_scan(conn, seconds):
    conn.simple("DROP TABLE IF EXISTS bench_scan")
    r = conn.simple("CREATE TABLE bench_scan(id INT, name TEXT, active BOOL)")
    assert tag_of(r) == "CREATE TABLE"
    # fill 10k rows, 500 per INSERT
    rows = []
    for i in range(10000):
        rows.append(f"({i},'name{i}',{'true' if i % 2 == 0 else 'false'})")
    for j in range(0, len(rows), 500):
        chunk = ",".join(rows[j:j + 500])
        r = conn.simple(f"INSERT INTO bench_scan VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(500, len(rows) - j)}", tag_of(r)

    def op():
        msgs = conn.simple("SELECT * FROM bench_scan")
        assert tag_of(msgs) == "SELECT 10000", tag_of(msgs)
    res = measure(op, seconds)
    conn.simple("DROP TABLE bench_scan")
    res["note"] = "10k-row full scan, one SELECT * per op"
    return res


def w_insert(conn, seconds):
    conn.simple("DROP TABLE IF EXISTS bench_ins")
    r = conn.simple("CREATE TABLE bench_ins(id INT, name TEXT, active BOOL)")
    assert tag_of(r) == "CREATE TABLE"
    # one 1000-row VALUES batch, reused every iteration
    rows = ",".join(
        f"({i},'name{i}',{'true' if i % 2 == 0 else 'false'})" for i in range(1000)
    )
    sql = f"INSERT INTO bench_ins VALUES {rows}"

    def op():
        msgs = conn.simple(sql)
        assert tag_of(msgs) == "INSERT 0 1000", tag_of(msgs)
    res = measure(op, seconds)
    res["rows_per_s"] = res["qps"] * 1000
    conn.simple("DROP TABLE bench_ins")
    res["note"] = "1000-row multi-VALUES INSERT per op"
    return res


def w_txn(conn, seconds):
    conn.simple("DROP TABLE IF EXISTS bench_txn")
    r = conn.simple("CREATE TABLE bench_txn(id INT, v TEXT)")
    assert tag_of(r) == "CREATE TABLE"

    def op():
        msgs = conn.simple("BEGIN")
        assert tag_of(msgs) == "BEGIN", tag_of(msgs)
        msgs = conn.simple("INSERT INTO bench_txn VALUES (1, 'x')")
        assert tag_of(msgs) == "INSERT 0 1", tag_of(msgs)
        msgs = conn.simple("COMMIT")
        assert tag_of(msgs) == "COMMIT", tag_of(msgs)
    res = measure(op, seconds)
    res["note"] = ("BEGIN + single-row INSERT + COMMIT per op; measures the "
                   "MVCC commit path (WAL fsync + snapshot bookkeeping)")
    conn.simple("DROP TABLE bench_txn")
    return res


BENCH_HOST = DEFAULT_HOST
BENCH_PORT = DEFAULT_PORT


def w_mvcc(conn, seconds, nthreads=4):
    """Concurrent MVCC workload: N threads x connections, each running
    BEGIN + UPDATE (own row) + SELECT + COMMIT in a loop. Threads touch
    disjoint rows so no write-write conflicts are expected; this measures
    snapshot/visibility/version-chain overhead under real concurrency on
    the shared engine."""
    conn.simple("DROP TABLE IF EXISTS bench_mvcc")
    r = conn.simple("CREATE TABLE bench_mvcc(id INT, v INT)")
    assert tag_of(r) == "CREATE TABLE"
    r = conn.simple(
        "INSERT INTO bench_mvcc VALUES " +
        ",".join(f"({i},0)" for i in range(nthreads)))
    assert tag_of(r) == f"INSERT 0 {nthreads}", tag_of(r)

    stop = threading.Event()
    lat_lock = threading.Lock()
    lat = []

    def worker(tid):
        c = Conn(BENCH_HOST, BENCH_PORT)
        try:
            # warmup
            end = time.perf_counter() + 1.0
            while time.perf_counter() < end and not stop.is_set():
                c.simple("BEGIN")
                c.simple(f"UPDATE bench_mvcc SET v = v + 1 WHERE id = {tid}")
                c.simple(f"SELECT v FROM bench_mvcc WHERE id = {tid}")
                c.simple("COMMIT")
            # measured
            local = []
            end = time.perf_counter() + seconds
            while time.perf_counter() < end and not stop.is_set():
                t0 = time.perf_counter()
                msgs = c.simple("BEGIN")
                assert tag_of(msgs) == "BEGIN", tag_of(msgs)
                msgs = c.simple(f"UPDATE bench_mvcc SET v = v + 1 WHERE id = {tid}")
                assert tag_of(msgs) == "UPDATE 1", tag_of(msgs)
                msgs = c.simple(f"SELECT v FROM bench_mvcc WHERE id = {tid}")
                assert tag_of(msgs) == "SELECT 1", tag_of(msgs)
                msgs = c.simple("COMMIT")
                assert tag_of(msgs) == "COMMIT", tag_of(msgs)
                local.append((time.perf_counter() - t0) * 1000.0)
            with lat_lock:
                lat.extend(local)
        finally:
            c.close()

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(nthreads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    conn.simple("DROP TABLE bench_mvcc")

    lat.sort()
    total_s = sum(lat) / 1000.0
    return {
        "ops": len(lat),
        "qps": len(lat) / total_s if total_s > 0 else 0.0,
        "p50_ms": percentile(lat, 50),
        "p99_ms": percentile(lat, 99),
        "note": (f"{nthreads} threads x (BEGIN + UPDATE + SELECT + COMMIT) "
                 "on disjoint rows; measures MVCC under concurrency"),
    }


def _parse_ok(conn, query, oids):
    try:
        msgs = conn.ext([m_parse("", query, oids)])
    except (RuntimeError, socket.error):
        return False
    if has_error(msgs):
        return False
    return any(t == b"1" for t, _ in msgs)


def w_prepared(conn, seconds):
    # Detect extended-protocol support. Prefer a $1-param statement, fall
    # back to a paramless one, else skip the workload gracefully.
    use_params = _parse_ok(conn, "SELECT $1", (25,))
    if not use_params and not _parse_ok(conn, "SELECT 1", ()):
        return {"skipped": "server did not answer Parse (extended protocol unsupported)"}
    conn.ext([m_close(b"S", "")])  # clean slate for the unnamed statement

    if use_params:
        conn.ext([m_parse("", "SELECT $1", (25,))])
        def op():
            msgs = conn.ext([m_bind("", "", ("hello",)), m_execute("", 0)])
            assert not has_error(msgs), "Bind/Execute failed"
        note = "Parse once; Bind($1='hello')/Execute per op"
    else:
        conn.ext([m_parse("", "SELECT 1", ())])
        def op():
            msgs = conn.ext([m_bind("", ""), m_execute("", 0)])
            assert not has_error(msgs), "Bind/Execute failed"
        note = "Parse once; Bind/Execute per op (no params)"
    res = measure(op, seconds)
    res["note"] = note
    return res


def w_join(conn, seconds):
    conn.simple("DROP TABLE IF EXISTS bench_o")
    conn.simple("DROP TABLE IF EXISTS bench_u")
    r = conn.simple("CREATE TABLE bench_u(id INT, name TEXT)")
    assert tag_of(r) == "CREATE TABLE"
    r = conn.simple("CREATE TABLE bench_o(id INT, uid INT, amt INT)")
    assert tag_of(r) == "CREATE TABLE"
    # 2k users, 20k orders (10 per user), 2000-row INSERT batches
    rows = ",".join(f"({i},'u{i}')" for i in range(2000))
    r = conn.simple(f"INSERT INTO bench_u VALUES {rows}")
    assert tag_of(r) == "INSERT 0 2000", tag_of(r)
    for j in range(0, 20000, 2000):
        chunk = ",".join(f"({i},{i%2000},{i%100})" for i in range(j, j + 2000))
        r = conn.simple(f"INSERT INTO bench_o VALUES {chunk}")
        assert tag_of(r) == "INSERT 0 2000", tag_of(r)

    sql = ("SELECT u.name, count(o.id), sum(o.amt) FROM bench_u u "
           "JOIN bench_o o ON u.id = o.uid WHERE u.id < 200 "
           "GROUP BY u.name ORDER BY u.name")

    def op():
        msgs = conn.simple(sql)
        assert tag_of(msgs) == "SELECT 200", tag_of(msgs)
    res = measure(op, seconds)
    conn.simple("DROP TABLE bench_o")
    conn.simple("DROP TABLE bench_u")
    res["note"] = ("2k x 20k nested-loop join (WHERE u.id < 200 pushed below "
                   "the join: 200 x 20k pairs), hash GROUP BY + aggregate "
                   "ORDER BY; one query per op")
    return res


def w_expr(conn, seconds):
    # v0.7: expression-heavy SELECT exercising the new types, casts,
    # operators and built-ins: numeric arithmetic + power, string
    # functions, math built-ins, CASE, casts, LIKE. One row out.
    sql = (
        "SELECT "
        "upper(substring('hello world', 1, 5)) || '-' || lower('ABC'), "
        "abs(-42) + round(3.14159, 2) * power(2, 10) - sqrt(16.0), "
        "('12345678901234567890.12345678'::numeric "
        "  * 2::numeric + 0.5::numeric) / 3::numeric, "
        "coalesce(NULL, 'fallback') || '-' || trim('  padded  '), "
        "'2026-09-10'::date + 30, "
        "position('ll' in 'hello') + char_length('rustgres'), "
        "split_part('a,b,c,d', ',', 3), "
        "'abc' LIKE 'a%' AND 'ABC' ILIKE 'a%', "
        "2 ^ 10 + 10 % 3"
    )

    def op():
        msgs = conn.simple(sql)
        assert tag_of(msgs) == "SELECT 1", tag_of(msgs)
    res = measure(op, seconds)
    res["note"] = ("single-row SELECT with numeric/string/math/date "
                   "expressions, casts, CASE, LIKE; one query per op")
    return res


def w_idxscan(conn, seconds):
    """v0.8: indexed vs sequential access paths on a 50k-row table."""
    conn.simple("DROP TABLE IF EXISTS bench_idx")
    r = conn.simple("CREATE TABLE bench_idx(id INT, grp INT, name TEXT)")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(50000):
        rows.append(f"({i},{i % 100},'name{i}')")
    for j in range(0, len(rows), 1000):
        chunk = ",".join(rows[j:j + 1000])
        r = conn.simple(f"INSERT INTO bench_idx VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(1000, len(rows) - j)}", tag_of(r)
    r = conn.simple("CREATE INDEX bench_idx_id ON bench_idx(id)")
    assert tag_of(r) == "CREATE INDEX"

    import random
    random.seed(42)

    def op_point_idx():
        i = random.randrange(50000)
        msgs = conn.simple(f"SELECT * FROM bench_idx WHERE id = {i}")
        assert tag_of(msgs) == "SELECT 1", tag_of(msgs)

    def op_range_idx():
        lo = random.randrange(49000)
        msgs = conn.simple(f"SELECT * FROM bench_idx WHERE id BETWEEN {lo} AND {lo + 999}")
        assert tag_of(msgs) == "SELECT 1000", tag_of(msgs)

    def op_order_idx():
        msgs = conn.simple("SELECT * FROM bench_idx ORDER BY id LIMIT 10")
        assert tag_of(msgs) == "SELECT 10", tag_of(msgs)

    res_point = measure(op_point_idx, seconds)
    res_range = measure(op_range_idx, seconds)
    res_order = measure(op_order_idx, seconds)

    # same lookups without the index
    conn.simple("DROP INDEX bench_idx_id")

    def op_point_seq():
        i = random.randrange(50000)
        msgs = conn.simple(f"SELECT * FROM bench_idx WHERE id = {i}")
        assert tag_of(msgs) == "SELECT 1", tag_of(msgs)

    res_seq = measure(op_point_seq, seconds)
    conn.simple("DROP TABLE bench_idx")

    res = res_point
    res["note"] = (
        "50k rows; point lookup qps idx=%.0f vs seq=%.0f (%.1fx); "
        "range-1000 idx=%.0f qps; order-limit-10 idx=%.0f qps" % (
            res_point["qps"], res_seq["qps"],
            res_point["qps"] / res_seq["qps"] if res_seq["qps"] else 0,
            res_range["qps"], res_order["qps"]))
    return res


WORKLOADS = {
    "select1": ("simple-query SELECT 1", w_select1),
    "expr": ("expression-heavy SELECT (v0.7 types/ops/built-ins)", w_expr),
    "scan": ("SELECT * over 10k rows", w_scan),
    "insert": ("1000-row batched INSERTs", w_insert),
    "prepared": ("extended-protocol prepared loop", w_prepared),
    "txn": ("BEGIN + INSERT + COMMIT loop", w_txn),
    "mvcc": ("concurrent MVCC txn loop (4 threads)", w_mvcc),
    "join": ("filtered JOIN + GROUP BY (2k x 20k)", w_join),
    "idxscan": ("indexed vs sequential point/range/order scans (50k rows)", w_idxscan),
}


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="rustgres wire-protocol benchmarks")
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--seconds", type=float, default=5,
                    help="measurement time per workload (default 5)")
    ap.add_argument("--workload", choices=list(WORKLOADS) + ["all"],
                    default="all", help="which workload to run")
    args = ap.parse_args()

    global BENCH_HOST, BENCH_PORT
    BENCH_HOST, BENCH_PORT = args.host, args.port
    names = list(WORKLOADS) if args.workload == "all" else [args.workload]
    print(f"rustgres bench: {args.host}:{args.port}, "
          f"{args.seconds:g}s per workload\n")

    results = {}
    for name in names:
        desc, fn = WORKLOADS[name]
        print(f"== {name}: {desc} ==")
        try:
            conn = Conn(args.host, args.port)
        except OSError as e:
            print(f"  cannot connect: {e}")
            results[name] = {"error": str(e)}
            continue
        try:
            res = fn(conn, args.seconds)
        except (AssertionError, RuntimeError, socket.error) as e:
            res = {"error": str(e)}
        finally:
            conn.close()
        results[name] = res
        if "skipped" in res:
            print(f"  skipped: {res['skipped']}")
        elif "error" in res:
            print(f"  ERROR: {res['error']}")
        else:
            extra = ""
            if "rows_per_s" in res:
                extra = f"  rows/s: {res['rows_per_s']:,.0f}\n"
            note = f"  note: {res['note']}\n" if "note" in res else ""
            print(f"  ops: {res['ops']}  qps: {res['qps']:,.1f}  "
                  f"p50: {res['p50_ms']:.3f} ms  p99: {res['p99_ms']:.3f} ms\n"
                  f"{extra}{note}")

    print("== summary ==")
    for name in names:
        r = results[name]
        if "qps" in r:
            print(f"  {name:10s} {r['qps']:12,.1f} qps   "
                  f"p50 {r['p50_ms']:.3f} ms   p99 {r['p99_ms']:.3f} ms")
        else:
            print(f"  {name:10s} {r.get('skipped', r.get('error', '?'))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
