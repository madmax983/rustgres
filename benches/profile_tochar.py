#!/usr/bin/env python3
"""Fixed-iteration-count driver for a `to_char` report-formatting workload.

Same rationale as profile_regexp.py/profile_join.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed number of rows once, then sends an exact COUNT of
the same `to_char`-heavy SELECT over the real wire protocol.

The query formats a price (currency), a quantity (grouped thousands),
and a percentage with three `to_char` calls, each with its own literal
format picture reused across every row and every iteration -- the same
shape as a real invoice/report export query that renders NUMERIC columns
for display. Not a synthetic microbenchmark of the formatting engine
alone: it drives the real `run_statement` -> `project_row` ->
`eval_func` -> `num_to_char`/`int_to_char` call path through the wire
protocol, like every other SELECT in this file's workloads.

No prior Bolt round in this repo has profiled `src/numfmt.rs` /
`src/numfmt_tochar.rs` (the `to_char`/`to_number` numeric-formatting
engine, v0.26) -- every previous round targeted the tokenizer/parser,
row send/project, join/order-by, aggregate, window, regexp, or TOAST
paths.

Usage:
    cargo build && ./target/debug/rustgres &                   # terminal 1
    python3 benches/profile_tochar.py --rows 3000 --count 20   # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_tochar.py --rows 3000 --count 20
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
    ap.add_argument("--rows", type=int, default=3000,
                     help="rows in bench_tc (default 3000)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_tc")
    r = conn.simple(
        "CREATE TABLE bench_tc(id INT, price NUMERIC, qty INT, pct NUMERIC)")
    assert tag_of(r) == "CREATE TABLE"

    def row_vals(i):
        price_cents = (i * 137 + 50) % 100_000_000
        price = f"{price_cents // 100}.{price_cents % 100:02d}"
        qty = (i * 7919) % 100_000
        pct_bps = (i * 31) % 10_000
        pct = f"{pct_bps // 100}.{pct_bps % 100:02d}"
        return f"({i},{price},{qty},{pct})"

    for j in range(0, args.rows, 1000):
        n = min(1000, args.rows - j)
        vals = ",".join(row_vals(i) for i in range(j, j + n))
        r = conn.simple(f"INSERT INTO bench_tc VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    sql = (
        "SELECT to_char(price, 'FM$999,999,999.00'), "
        "to_char(qty, 'FM999,999'), "
        "to_char(pct, 'FM990.99') "
        "FROM bench_tc"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a 3-call to_char report-format "
        f"scan over {args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
