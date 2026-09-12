#!/usr/bin/env python3
"""Fixed-iteration-count driver for deterministic profiling.

bench.py's workloads are time-boxed (run for N seconds), which is the
right choice for reporting qps but makes two profiling runs (e.g. a
before/after callgrind or dhat comparison) execute a *different* number
of queries whenever the machine's wall-clock speed jitters even slightly
under valgrind. That makes instruction/allocation counts across runs
incomparable.

This driver instead sends a fixed COUNT of the same query through the
real wire protocol (simple-query, same code path as `bench.py --workload
expr`), so two profiling runs do identical work and any counter delta is
attributable to the code change under test, not to how many iterations
happened to fit in 5 seconds.

Usage:
    cargo build && ./target/debug/rustgres &         # terminal 1
    python3 benches/profile_fixed.py --count 3000    # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_fixed.py --count 3000
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

# Same query as bench.py's w_expr: exercises the tokenizer/parser plus
# v0.7 numeric/string/math/date/cast/LIKE evaluation on every call.
EXPR_SQL = (
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


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--count", type=int, default=3000,
                    help="exact number of queries to send (default 3000)")
    ap.add_argument("--sql", default=None,
                    help="override query text (default: the expr workload query)")
    ap.add_argument("--setup", action="append", default=[],
                    help="statement(s) to run once before the loop (e.g. CREATE TABLE)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    for stmt in args.setup:
        conn.simple(stmt)
    sql = args.sql if args.sql is not None else EXPR_SQL
    for _ in range(args.count):
        conn.simple(sql)
    print(f"ran {args.count} fixed iterations", file=sys.stderr)


if __name__ == "__main__":
    main()
