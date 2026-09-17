#!/usr/bin/env python3
"""Fixed-iteration-count driver for the indexed range-scan workload.

Same rationale as profile_join.py/profile_insert.py/profile_toast.py:
bench.py's `idxscan` workload is time-boxed, so two profiling runs execute
a different number of queries whenever wall-clock speed jitters under
valgrind, which would confound a before/after instruction/allocation
comparison. This driver instead loads a fixed 50k-row table with a
secondary index once (same shape as bench.py's `w_idxscan`), then sends an
exact COUNT of `WHERE id BETWEEN lo AND lo+width-1` range queries over the
real wire protocol against that index, so two profiling runs do identical
work.

No prior Bolt round has profiled src/index.rs or the index-scan path in
src/exec.rs.

Usage:
    cargo build && ./target/debug/rustgres &         # terminal 1
    python3 benches/profile_idxscan.py --count 50 --width 1000  # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_idxscan.py --count 50 --width 1000
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
    ap.add_argument("--rows", type=int, default=50000,
                     help="rows to load into bench_idxscan before measuring")
    ap.add_argument("--count", type=int, default=50,
                     help="exact number of BETWEEN range-scan iterations")
    ap.add_argument("--width", type=int, default=1000,
                     help="row width of each BETWEEN range")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "default 30s from bench.py's Conn is too short)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_idxscan")
    r = conn.simple("CREATE TABLE bench_idxscan(id INT, grp INT, name TEXT)")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(args.rows):
        rows.append(f"({i},{i % 100},'name{i}')")
    for j in range(0, len(rows), 1000):
        chunk = ",".join(rows[j:j + 1000])
        r = conn.simple(f"INSERT INTO bench_idxscan VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(1000, len(rows) - j)}", tag_of(r)
    r = conn.simple("CREATE INDEX bench_idxscan_id ON bench_idxscan(id)")
    assert tag_of(r) == "CREATE INDEX"

    # Deterministic sequence of ranges: walk evenly across the key space so
    # every iteration hits a fresh, non-cached span, same as the real
    # workload's `random.randrange`, but reproducible run to run.
    span = max(args.rows - args.width, 1)
    step = max(span // max(args.count, 1), 1)
    for k in range(args.count):
        lo = (k * step) % span
        hi = lo + args.width - 1
        msgs = conn.simple(f"SELECT * FROM bench_idxscan WHERE id BETWEEN {lo} AND {hi}")
        assert tag_of(msgs) == f"SELECT {args.width}", tag_of(msgs)
    print(f"ran {args.count} BETWEEN range-scan iterations (width {args.width}) "
          f"over {args.rows} rows", file=sys.stderr)


if __name__ == "__main__":
    main()
