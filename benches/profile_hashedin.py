#!/usr/bin/env python3
"""Fixed-iteration-count driver for `WHERE col IN (subquery)` (v1.19's
hashed IN-subquery fast path, `eval_hashed_in` in src/exec.rs).

Same rationale as profile_join.py: bench.py's workloads are time-boxed, so
two profiling runs execute a different amount of work whenever wall-clock
speed jitters under valgrind. This driver loads a fixed outer/inner table
size once, then sends an exact COUNT of the same
`SELECT count(*) FROM bench_outer WHERE k IN (SELECT id FROM bench_inner
WHERE tag = 'x')` query over the real wire protocol.

The IN-subquery is uncorrelated and single-table, matching v1.19's
`match_hashable_in` shape, so the fast path in `eval_hashed_in` runs once
per OUTER ROW (not once per query): the subquery itself executes once
(cached), but the shape-matching / correlation-safety checks
(`match_hashable_in`, `subplan_inner_is_base_table`,
`subquery_refs_only_inner`) and the cache lookup re-run for every outer
row's `IN` evaluation. This workload's outer table is sized to make that
per-row overhead, not the one-time subquery execution, dominate.

Usage:
    cargo build && ./target/debug/rustgres &                       # terminal 1
    python3 benches/profile_hashedin.py --outer-rows 50000 --count 20  # terminal 2
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
    ap.add_argument("--outer-rows", type=int, default=50000,
                     help="rows in bench_outer (default 50000)")
    ap.add_argument("--inner-rows", type=int, default=500,
                     help="rows in bench_inner (default 500)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_outer")
    conn.simple("DROP TABLE IF EXISTS bench_inner")
    r = conn.simple("CREATE TABLE bench_outer(id INT, k INT)")
    assert tag_of(r) == "CREATE TABLE"
    r = conn.simple("CREATE TABLE bench_inner(id INT, tag TEXT)")
    assert tag_of(r) == "CREATE TABLE"

    for j in range(0, args.outer_rows, 2000):
        n = min(2000, args.outer_rows - j)
        chunk = ",".join(f"({i},{i % (args.inner_rows * 2)})" for i in range(j, j + n))
        r = conn.simple(f"INSERT INTO bench_outer VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    inner_rows = ",".join(
        f"({i},'{'x' if i % 2 == 0 else 'y'}')" for i in range(args.inner_rows)
    )
    r = conn.simple(f"INSERT INTO bench_inner VALUES {inner_rows}")
    assert tag_of(r) == f"INSERT 0 {args.inner_rows}", tag_of(r)

    sql = ("SELECT count(*) FROM bench_outer WHERE k IN "
           "(SELECT id FROM bench_inner WHERE tag = 'x')")

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == "SELECT 1", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a {args.outer_rows}-row hashed "
        f"IN-subquery ({args.inner_rows}-row inner)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
