#!/usr/bin/env python3
"""Fixed-iteration-count driver for a full-table-scan SELECT * workload.

Same rationale as profile_fixed.py: bench.py's `scan` workload is
time-boxed, so two profiling runs execute a different number of queries
whenever wall-clock speed jitters under valgrind, which would confound a
before/after instruction/allocation comparison. This driver instead loads
a fixed number of rows once, then sends an exact COUNT of `SELECT *`
queries over the real wire protocol, so two profiling runs do identical
work.

Usage:
    cargo build && ./target/debug/rustgres &         # terminal 1
    python3 benches/profile_scan.py --rows 10000 --count 30   # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=dhat ./target/debug/rustgres &
    python3 benches/profile_scan.py --rows 10000 --count 30
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
    ap.add_argument("--rows", type=int, default=10000,
                     help="rows to load into bench_scan before measuring")
    ap.add_argument("--count", type=int, default=30,
                     help="exact number of SELECT * iterations")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.simple("DROP TABLE IF EXISTS bench_scan")
    r = conn.simple("CREATE TABLE bench_scan(id INT, name TEXT, active BOOL)")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(args.rows):
        rows.append(f"({i},'name{i}',{'true' if i % 2 == 0 else 'false'})")
    for j in range(0, len(rows), 500):
        chunk = ",".join(rows[j:j + 500])
        r = conn.simple(f"INSERT INTO bench_scan VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(500, len(rows) - j)}", tag_of(r)

    for _ in range(args.count):
        msgs = conn.simple("SELECT * FROM bench_scan")
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(f"ran {args.count} SELECT * iterations over {args.rows} rows", file=sys.stderr)


if __name__ == "__main__":
    main()
