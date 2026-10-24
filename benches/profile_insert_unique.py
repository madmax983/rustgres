#!/usr/bin/env python3
"""Fixed-size driver for a single multi-row INSERT into a table that has
a PRIMARY KEY — i.e. almost every real table. This is the common
bulk-load shape: a loader (ETL job, migration script, ORM batch insert,
`COPY`-adjacent multi-VALUES INSERT) sends one `INSERT INTO t VALUES
(...), (...), ...` statement carrying many rows into a table whose
PRIMARY KEY must be checked for both in-batch duplicates and conflicts
with rows already in the table.

Unlike `benches/profile_insert.py` (which targets a PK-less table and so
never exercises the unique-constraint path at all), this harness's table
has `id INT PRIMARY KEY`, putting `check_insert_unique`'s per-statement
duplicate check on the hot path.

`--rows` controls the input size; run this at 3+ sizes (default set in
`--sizes`, used by `run_sizes`) to see whether the statement's cost
scales linearly or worse with the batch size — the asymptotic-complexity
argument from a single callgrind run per size, not a wall-clock timing.

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_insert_unique.py --rows 2000        # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_insert_unique.py --rows 2000
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
                     help="rows in the single multi-VALUES INSERT "
                          "(default 2000)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_ins_pk")
    r = conn.simple(
        "CREATE TABLE bench_ins_pk(id INT PRIMARY KEY, name TEXT, "
        "active BOOL)"
    )
    assert tag_of(r) == "CREATE TABLE"

    rows = ",".join(
        f"({i},'name{i}',{'true' if i % 2 == 0 else 'false'})"
        for i in range(args.rows)
    )
    sql = f"INSERT INTO bench_ins_pk VALUES {rows}"

    msgs = conn.simple(sql)
    assert tag_of(msgs) == f"INSERT 0 {args.rows}", tag_of(msgs)

    print(
        f"ran one {args.rows}-row multi-VALUES INSERT into a "
        "PRIMARY KEY table",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
