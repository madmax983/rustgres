#!/usr/bin/env python3
"""Fixed-iteration-count driver for a batched multi-row INSERT workload.

Same rationale as profile_join.py/profile_scan.py: bench.py's `insert`
workload is time-boxed, so two profiling runs execute a different amount
of work whenever wall-clock speed jitters under valgrind, confounding a
before/after instruction/allocation comparison. This driver sends an exact
COUNT of the same 1000-row multi-VALUES INSERT bench.py's `insert`
workload uses, over the real wire protocol, against a fresh table each
time (so every run does identical work: no accumulating table growth
across iterations skewing later iterations' cost).

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_insert.py --count 20               # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_insert.py --count 20
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
    ap.add_argument("--rows", type=int, default=1000,
                     help="rows per multi-VALUES INSERT (default 1000, "
                          "matching bench.py's insert workload)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of INSERT statements")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_ins")
    r = conn.simple("CREATE TABLE bench_ins(id INT, name TEXT, active BOOL)")
    assert tag_of(r) == "CREATE TABLE"

    rows = ",".join(
        f"({i},'name{i}',{'true' if i % 2 == 0 else 'false'})"
        for i in range(args.rows)
    )
    sql = f"INSERT INTO bench_ins VALUES {rows}"

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"INSERT 0 {args.rows}", tag_of(msgs)

    print(
        f"ran {args.count} iterations of a {args.rows}-row multi-VALUES INSERT",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
