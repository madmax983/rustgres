#!/usr/bin/env python3
"""Fixed-iteration-count driver for a SELECT scan workload running
alongside a fixed number of other connections that are idle in an open
transaction.

Every MVCC visibility check in `src/storage.rs` (`row_visible`,
`table_visible`, `seq_visible`, `range_visible`, and friends) tests
`snap.active.contains(&xid)` against `Snapshot.active: Vec<u64>` -- a
linear scan over the xids of every transaction that was active (BEGIN'd
but not yet COMMIT/ROLLBACK) when the snapshot was taken. Every prior
Bolt profiling round in this repo drove its workload through exactly one
connection, so `active` held 0-1 entries throughout and this scan was
free; none has varied the number of *other, concurrently open*
transactions.

"Many connections sitting idle in an open transaction" is an extremely
common real-world shape: a connection-pooled app (PgBouncer in session
mode, an ORM that opens a transaction per request and holds it across
other work, a forgotten `BEGIN` from a REPL) routinely has tens to
hundreds of sessions parked mid-transaction while other sessions keep
issuing ordinary queries. This driver reproduces that directly through
the public wire protocol: it opens `--idle-txns` separate connections,
sends a bare `BEGIN` on each (registering the xid in
`TxnManager::active` via `begin_txn`, the same path any real client's
`BEGIN` takes) and leaves them open (blocked on `recv`, never
committing), then runs the real workload -- `--count` iterations of
`SELECT * FROM bench_snap` over a `--rows`-row table -- on one more,
separate connection.

Each idle connection's setup (`BEGIN`) completes synchronously before
the next one is opened and before the measured workload starts, so by
the time the workload runs, the `--idle-txns` threads are blocked on a
blocking `recv()` with no pending data -- not runnable, not scheduled,
contributing no further instructions. The measured phase is therefore
still effectively single-threaded work from the scanning connection's
point of view, with a `TxnManager::active` HashSet of a controlled,
realistic size.

Usage:
    cargo build && ./target/debug/rustgres &
    python3 benches/profile_snapshot_active.py --rows 2000 --count 10 --idle-txns 100
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_snapshot_active.py --rows 2000 --count 10 --idle-txns 100
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=2000,
                     help="rows to load into bench_snap before measuring")
    ap.add_argument("--count", type=int, default=10,
                     help="exact number of SELECT * iterations to measure")
    ap.add_argument("--idle-txns", type=int, default=0,
                     help="number of other connections to leave idle in an "
                          "open transaction (BEGIN, never committed) while "
                          "the workload runs")
    args = ap.parse_args()

    setup = Conn(args.host, args.port)
    setup.simple("DROP TABLE IF EXISTS bench_snap")
    r = setup.simple("CREATE TABLE bench_snap(id INT, val INT, name TEXT)")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(args.rows):
        rows.append(f"({i},{i * 7 % 997},'name{i}')")
    for j in range(0, len(rows), 500):
        chunk = ",".join(rows[j:j + 500])
        r = setup.simple(f"INSERT INTO bench_snap VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(500, len(rows) - j)}", tag_of(r)

    # Open and BEGIN each idle connection synchronously, one at a time, so
    # none of this setup overlaps with another connection's work.
    idle_conns = []
    for _ in range(args.idle_txns):
        c = Conn(args.host, args.port)
        r = c.simple("BEGIN")
        assert tag_of(r) == "BEGIN", tag_of(r)
        idle_conns.append(c)

    driver = Conn(args.host, args.port)
    for _ in range(args.count):
        msgs = driver.simple("SELECT * FROM bench_snap")
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)

    print(f"ran {args.count} SELECT * iterations over {args.rows} rows "
          f"with {args.idle_txns} other connections idle in an open "
          f"transaction", file=sys.stderr)

    driver.close()
    for c in idle_conns:
        try:
            c.simple("ROLLBACK")
            c.close()
        except OSError:
            pass
    setup.close()


if __name__ == "__main__":
    main()
