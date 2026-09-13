#!/usr/bin/env python3
"""Fixed-iteration-count driver for a numeric ORDER BY workload.

Same rationale as profile_scan.py/profile_join.py: bench.py's `idxscan`
workload (which includes an `ORDER BY id LIMIT 10` op) is time-boxed, so two
profiling runs execute a different amount of work whenever wall-clock speed
jitters under valgrind, confounding a before/after instruction/allocation
comparison. This driver instead loads a fixed number of rows into a table
with a plain INT column once, then sends an exact COUNT of
`ORDER BY <int col> LIMIT 10` queries over the real wire protocol, so two
profiling runs do identical work.

No index is created on the sort column: the goal is to exercise the
row-sort comparator (`exec::compare_values`) directly, not
`plan_access_path`'s index-scan-for-ordering decision (a separate code
path this driver is not profiling).

Usage:
    cargo build && ./target/debug/rustgres &                     # terminal 1
    python3 benches/profile_orderby.py --rows 20000 --count 30   # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_orderby.py --rows 20000 --count 30
"""
import argparse
import os
import random
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=20000,
                     help="rows to load into bench_ord before measuring")
    ap.add_argument("--count", type=int, default=30,
                     help="exact number of ORDER BY ... LIMIT 10 iterations")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.simple("DROP TABLE IF EXISTS bench_ord")
    r = conn.simple("CREATE TABLE bench_ord(id INT, val INT)")
    assert tag_of(r) == "CREATE TABLE"

    # Shuffled, not ascending: an already-sorted input would let the sort's
    # comparator short-circuit into far fewer real comparisons on some
    # implementations, and would not represent a realistic unordered table.
    order = list(range(args.rows))
    random.seed(42)
    random.shuffle(order)

    rows = [f"({i},{v})" for i, v in enumerate(order)]
    for j in range(0, len(rows), 1000):
        chunk = ",".join(rows[j:j + 1000])
        r = conn.simple(f"INSERT INTO bench_ord VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(1000, len(rows) - j)}", tag_of(r)

    for _ in range(args.count):
        msgs = conn.simple("SELECT * FROM bench_ord ORDER BY val LIMIT 10")
        assert tag_of(msgs) == "SELECT 10", tag_of(msgs)
    print(
        f"ran {args.count} ORDER BY ... LIMIT 10 iterations over {args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
