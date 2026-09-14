#!/usr/bin/env python3
"""Fixed-iteration-count driver for the explicit-column-list projection path.

`project_row`'s `SELECT *` shape takes a zero-allocation fast path (the
row's cell Vec moves through untouched — see the early return at the top
of `project_row` in src/exec.rs). An explicit column list (`SELECT a, b,
c, ...`) instead builds a fresh `Vec::new()` output buffer and pushes one
cell per select item, which BASELINE.md flags as an un-sized allocation
(two growth reallocations per row) but has never actually been profiled.

This driver loads a fixed row count into a multi-column table once, then
sends an exact COUNT of `SELECT <explicit column list> FROM ...` queries
over the real wire protocol, so two profiling runs (before/after a
capacity-hint fix) execute identical work.

Usage:
    cargo build && ./target/debug/rustgres &
    python3 benches/profile_project.py --rows 20000 --count 30
    # or under valgrind, same pattern as profile_scan.py:
    valgrind --tool=dhat --dhat-out-file=/tmp/dhat.out ./target/debug/rustgres &
    python3 benches/profile_project.py --rows 20000 --count 30
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

NCOLS = 8


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=20000,
                     help="rows to load into bench_proj before measuring")
    ap.add_argument("--count", type=int, default=30,
                     help="exact number of SELECT iterations")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.simple("DROP TABLE IF EXISTS bench_proj")
    cols = ",".join(f"c{i} INT" for i in range(NCOLS))
    r = conn.simple(f"CREATE TABLE bench_proj({cols})")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(args.rows):
        vals = ",".join(str(i + j) for j in range(NCOLS))
        rows.append(f"({vals})")
    for j in range(0, len(rows), 500):
        chunk = ",".join(rows[j:j + 500])
        r = conn.simple(f"INSERT INTO bench_proj VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(500, len(rows) - j)}", tag_of(r)

    select_list = ",".join(f"c{i}" for i in range(NCOLS))
    sql = f"SELECT {select_list} FROM bench_proj"
    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(f"ran {args.count} explicit-column-list SELECT iterations over "
          f"{args.rows} rows x {NCOLS} cols", file=sys.stderr)


if __name__ == "__main__":
    main()
