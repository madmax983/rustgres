#!/usr/bin/env python3
"""Fixed-iteration-count driver for the GROUP BY per-group output path.

`exec_agg`'s per-group output loop (`src/exec.rs`, the `for (fgi, &gi) in
surviving.iter().enumerate()` loop inside `exec_agg_one`) builds each
output row's cell buffer with a bare `let mut cells = Vec::new();` and
pushes one cell per select item -- the same unsized-growth shape
`project_row` had before its `Vec::with_capacity(out_ncols)` fix
(BASELINE.md, "`project_row`'s output-cell `Vec` grows unsized",
2026-09-14), which explicitly flagged this `exec_agg` site as an
unmeasured follow-up and left it untouched.

This driver loads a fixed-size table with many small groups once, then
sends an exact COUNT of a `GROUP BY` aggregate query (6 output columns:
the key plus 5 aggregates) over the real wire protocol, so two profiling
runs (before/after a capacity-hint fix) execute identical work.

Usage:
    cargo build && ./target/debug/rustgres &
    python3 benches/profile_agg.py --rows 40000 --groups 4000 --count 50
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
    ap.add_argument("--rows", type=int, default=40000,
                     help="rows to load into bench_agg before measuring")
    ap.add_argument("--groups", type=int, default=4000,
                     help="distinct GROUP BY key values (rows/groups rows "
                          "per group)")
    ap.add_argument("--count", type=int, default=50,
                     help="exact number of GROUP BY SELECT iterations")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.simple("DROP TABLE IF EXISTS bench_agg")
    r = conn.simple(
        "CREATE TABLE bench_agg(k INT, a INT, b INT, c INT, d INT)")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(args.rows):
        k = i % args.groups
        rows.append(f"({k},{i},{i * 2},{i % 97},{i % 13})")
    for j in range(0, len(rows), 500):
        chunk = ",".join(rows[j:j + 500])
        r = conn.simple(f"INSERT INTO bench_agg VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(500, len(rows) - j)}", tag_of(r)

    sql = ("SELECT k, count(*), sum(a), sum(b), max(c), min(d) "
           "FROM bench_agg GROUP BY k")
    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.groups}", tag_of(msgs)
    print(f"ran {args.count} GROUP BY iterations over {args.rows} rows / "
          f"{args.groups} groups", file=sys.stderr)


if __name__ == "__main__":
    main()
