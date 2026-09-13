#!/usr/bin/env python3
"""Fixed-iteration-count driver for a nested-loop equi-join workload.

Same rationale as profile_fixed.py/profile_scan.py: bench.py's `join`
workload is time-boxed, so two profiling runs execute a different amount
of work whenever wall-clock speed jitters under valgrind, confounding a
before/after instruction/allocation comparison. This driver loads a fixed
number of users/orders once, then sends an exact COUNT of the same
INNER JOIN query over the real wire protocol.

Same query shape as bench.py's `join` workload (users JOIN orders ON
id = uid, WHERE pushed onto the users side, GROUP BY + aggregate + ORDER
BY), scaled down by default so a callgrind/dhat run finishes in a
reasonable time; pass --users/--orders/--filter to match bench.py's
full-size 2000/20000/200 shape (much slower under valgrind).

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_join.py --count 100               # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_join.py --count 100
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
    ap.add_argument("--users", type=int, default=200,
                     help="rows in bench_u (default 200; bench.py's join uses 2000)")
    ap.add_argument("--orders", type=int, default=2000,
                     help="rows in bench_o (default 2000; bench.py's join uses 20000)")
    ap.add_argument("--filter", type=int, default=20,
                     help="WHERE u.id < FILTER, pushed below the join "
                          "(default 20; bench.py's join uses 200)")
    ap.add_argument("--count", type=int, default=100,
                     help="exact number of JOIN query iterations")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_o")
    conn.simple("DROP TABLE IF EXISTS bench_u")
    r = conn.simple("CREATE TABLE bench_u(id INT, name TEXT)")
    assert tag_of(r) == "CREATE TABLE"
    r = conn.simple("CREATE TABLE bench_o(id INT, uid INT, amt INT)")
    assert tag_of(r) == "CREATE TABLE"

    rows = ",".join(f"({i},'u{i}')" for i in range(args.users))
    r = conn.simple(f"INSERT INTO bench_u VALUES {rows}")
    assert tag_of(r) == f"INSERT 0 {args.users}", tag_of(r)
    for j in range(0, args.orders, 2000):
        n = min(2000, args.orders - j)
        chunk = ",".join(
            f"({i},{i % args.users},{i % 100})" for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_o VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    sql = ("SELECT u.name, count(o.id), sum(o.amt) FROM bench_u u "
           "JOIN bench_o o ON u.id = o.uid WHERE u.id < "
           f"{args.filter} GROUP BY u.name ORDER BY u.name")

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.filter}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a {args.filter} x {args.orders} "
        "nested-loop join",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
