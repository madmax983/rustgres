#!/usr/bin/env python3
"""Fixed-iteration-count driver for a plan-only-EXPLAIN-heavy workload.

Same rationale as profile_jsonpath.py/profile_join.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed, realistic multi-table schema once, then sends an
exact COUNT of `EXPLAIN` (no ANALYZE -- nothing executes, this isolates
plan-time cost) over a representative mix of query shapes.

v1.98 added a full static-analysis pass (`validate_explain_select` in
src/exec/explain.rs) that plan-only EXPLAIN now runs before planning:
privilege checks, FROM-schema construction, and recursive expression
type resolution over every clause (target list, WHERE, GROUP BY,
HAVING, ORDER BY) for every query level (subqueries, CTEs, join
conditions). That pass has never been profiled. The queries below are
representative of what a real client that inspects query plans before
running them would send: a join with a filtered/grouped aggregate (the
same shape as bench.py's `join` workload), a correlated-subquery
EXISTS filter, and a CTE -- the three FROM/subquery/CTE shapes
`validate_explain_select` recurses into.

Usage:
    cargo build && ./target/debug/rustgres &                   # terminal 1
    python3 benches/profile_explain.py --count 300              # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_explain.py --count 300
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

QUERIES = [
    # Join + filter + group + aggregate + order -- the bench.py `join` shape.
    "EXPLAIN SELECT c.name, count(o.id), sum(o.total) FROM bench_customers c "
    "JOIN bench_orders o ON c.id = o.customer_id "
    "WHERE c.region = 'west' AND o.status <> 'cancelled' "
    "GROUP BY c.name HAVING count(o.id) > 0 ORDER BY c.name",
    # Correlated-subquery EXISTS filter (its own query level).
    "EXPLAIN SELECT c.id, c.name FROM bench_customers c "
    "WHERE EXISTS (SELECT 1 FROM bench_orders o "
    "WHERE o.customer_id = c.id AND o.total > 100)",
    # CTE feeding a join (CTE body is its own analyzed level).
    "EXPLAIN WITH big_orders AS ("
    "  SELECT customer_id, total FROM bench_orders WHERE total > 50"
    ") SELECT c.name, bo.total FROM bench_customers c "
    "JOIN big_orders bo ON bo.customer_id = c.id ORDER BY bo.total DESC",
]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--customers", type=int, default=200,
                     help="rows in bench_customers (default 200)")
    ap.add_argument("--orders", type=int, default=2000,
                     help="rows in bench_orders (default 2000)")
    ap.add_argument("--count", type=int, default=300,
                     help="exact number of EXPLAIN round-trips per query "
                          "shape (default 300)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_orders")
    conn.simple("DROP TABLE IF EXISTS bench_customers")
    r = conn.simple(
        "CREATE TABLE bench_customers(id INT, name TEXT, region TEXT)"
    )
    assert tag_of(r) == "CREATE TABLE"
    r = conn.simple(
        "CREATE TABLE bench_orders(id INT, customer_id INT, status TEXT, "
        "total INT)"
    )
    assert tag_of(r) == "CREATE TABLE"

    regions = ["west", "east", "north", "south"]
    rows = ",".join(
        "({0},'cust-{0}','{1}')".format(i, regions[i % 4])
        for i in range(args.customers)
    )
    r = conn.simple(f"INSERT INTO bench_customers VALUES {rows}")
    assert tag_of(r) == f"INSERT 0 {args.customers}", tag_of(r)

    statuses = ["pending", "shipped", "cancelled"]
    for j in range(0, args.orders, 2000):
        n = min(2000, args.orders - j)
        chunk = ",".join(
            "({0},{1},'{2}',{3})".format(
                i, i % args.customers, statuses[i % 3], 10 + i % 400
            )
            for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_orders VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    total = 0
    for sql in QUERIES:
        for _ in range(args.count):
            msgs = conn.simple(sql)
            assert tag_of(msgs) == "EXPLAIN", tag_of(msgs)
            total += 1
    print(
        f"ran {total} EXPLAIN round-trips ({args.count} each of "
        f"{len(QUERIES)} query shapes) over {args.customers} customers x "
        f"{args.orders} orders",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
