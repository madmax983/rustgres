#!/usr/bin/env python3
"""Fixed-size driver for a single multi-row UPDATE against a table that has
a PRIMARY KEY -- i.e. almost every real table. This is the common bulk
mutation shape: `UPDATE orders SET status = 'shipped' WHERE ship_date <
'2026-01-01'`, a batch job updating a non-key column over thousands of
rows of a PK table in one statement.

Companion to benches/profile_insert_unique.py, which found and fixed the
identical O(batch_len^2) shape in check_insert_unique (see #61). The same
BASELINE.md noted check_update_unique_pairs as an unmeasured follow-up
with the identical pairwise-rescan shape for UPDATE's planned rows.

Setup (CREATE TABLE + one multi-VALUES INSERT to populate the table) is
not the target and is cheap relative to the UPDATE at the sizes used
here, but is included in the end-to-end instruction count like every
other Bolt harness in this tree; the targeted subtree's share of the
total is reported separately via callgrind_annotate.

`--rows` controls how many rows exist (and are all touched by the single
UPDATE); run at 3+ sizes (default set in `run_sizes`) to see whether the
statement's cost scales linearly or worse with row count.

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_update_unique.py --rows 2000        # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_update_unique.py --rows 2000
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
                     help="rows in the table, all touched by the single "
                          "UPDATE (default 2000)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_upd_pk")
    r = conn.simple(
        "CREATE TABLE bench_upd_pk(id INT PRIMARY KEY, name TEXT, "
        "val INT)"
    )
    assert tag_of(r) == "CREATE TABLE"

    rows = ",".join(
        f"({i},'name{i}',{i})"
        for i in range(args.rows)
    )
    r = conn.simple(f"INSERT INTO bench_upd_pk VALUES {rows}")
    assert tag_of(r) == f"INSERT 0 {args.rows}", tag_of(r)

    # Single UPDATE touching every row. Does not change the PRIMARY KEY
    # column, which is the common case -- check_update_unique_pairs still
    # runs its pairwise rescan against the PK index regardless of which
    # columns the SET clause actually changed.
    msgs = conn.simple("UPDATE bench_upd_pk SET val = val + 1")
    assert tag_of(msgs) == f"UPDATE {args.rows}", tag_of(msgs)

    print(
        f"ran one {args.rows}-row UPDATE (all rows) against a "
        "PRIMARY KEY table",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
