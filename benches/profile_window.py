#!/usr/bin/env python3
"""Fixed-iteration-count driver for a cumulative-window-function workload.

Same rationale as profile_scan.py/profile_join.py/profile_orderby.py:
bench.py's `window` workload is time-boxed, so two profiling runs execute a
different amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This driver
instead loads a fixed number of rows into a table with a partition column
and a value column once, then sends an exact COUNT of running-total queries
over the real wire protocol, so two profiling runs do identical work.

The query is a realistic "running balance / running count" report — the
single most common real-world use of window functions (a per-partition
cumulative sum and cumulative count, in ORDER BY id order, with the
default frame Postgres gives a windowed aggregate when it has an ORDER BY
and no explicit frame clause: RANGE UNBOUNDED PRECEDING AND CURRENT ROW):

    SELECT dept, id,
           sum(val) OVER (PARTITION BY dept ORDER BY id) AS running_sum,
           count(*) OVER (PARTITION BY dept ORDER BY id) AS running_count,
           count(val) OVER (PARTITION BY dept ORDER BY id) AS running_nonnull
    FROM bench_win

`--rows` splits evenly across `--partitions` partitions (default 10, like
the existing v0.10 `window` bench workload), so each partition holds
`rows / partitions` rows in strictly increasing `id` order (no ties, so
RANGE's peer-group walk in `resolve_frame` never has to skip more than one
row — the query exercises the cumulative-frame aggregation cost, not the
peer-walk).

Usage:
    cargo build && ./target/debug/rustgres &                       # terminal 1
    python3 benches/profile_window.py --rows 2000 --count 5        # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_window.py --rows 2000 --count 5
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
                     help="rows to load into bench_win before measuring")
    ap.add_argument("--partitions", type=int, default=10,
                     help="number of PARTITION BY groups (rows split evenly)")
    ap.add_argument("--count", type=int, default=5,
                     help="exact number of running-total query iterations")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.simple("DROP TABLE IF EXISTS bench_win")
    r = conn.simple("CREATE TABLE bench_win(id INT, dept INT, val INT)")
    assert tag_of(r) == "CREATE TABLE"

    rows = []
    for i in range(args.rows):
        dept = i % args.partitions
        val = (i * 7 + 3) % 1000
        rows.append(f"({i},{dept},{val})")
    for j in range(0, len(rows), 1000):
        chunk = ",".join(rows[j:j + 1000])
        r = conn.simple(f"INSERT INTO bench_win VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(1000, len(rows) - j)}", tag_of(r)

    query = (
        "SELECT dept, id, "
        "sum(val) OVER (PARTITION BY dept ORDER BY id) AS running_sum, "
        "count(*) OVER (PARTITION BY dept ORDER BY id) AS running_count, "
        "count(val) OVER (PARTITION BY dept ORDER BY id) AS running_nonnull "
        "FROM bench_win"
    )
    for _ in range(args.count):
        msgs = conn.simple(query)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} running-total iterations over {args.rows} rows "
        f"/ {args.partitions} partitions",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
