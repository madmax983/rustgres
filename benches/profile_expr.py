#!/usr/bin/env python3
"""Fixed-iteration-count driver for a text-producing projection workload.

Same rationale as profile_scan.py/profile_join.py: an exact iteration
count, so two profiling runs do identical work under valgrind.

This driver exists to guard the one path the scan and join drivers never
touch. Those two send `SELECT *` and join integer columns, so neither
builds a text value. `Value::Text` holds an `Arc<str>`, and an `Arc<str>`
always owns its buffer, so any path that builds a `String` and then makes
a text value copies the bytes one more time than a plain `String` would.
This driver makes that cost visible: an int-only table and five text
results for each output row.

    --mode cast     five `::text` casts  (exercises eval_cast)
    --mode lit      five text literals   (exercises Literal -> Value)

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_expr.py --count 20                # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=dhat ./target/debug/rustgres &
    python3 benches/profile_expr.py --count 20
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

QUERIES = {
    "cast": (
        "SELECT id::text, n::text, (id+1)::text, (n+1)::text, (id+n)::text "
        "FROM bench_expr"
    ),
    "lit": (
        "SELECT 'alpha_tag', 'beta_tag', 'gamma_tag', 'delta_tag', 'epsilon_tag' "
        "FROM bench_expr"
    ),
}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=2000,
                    help="rows to load into bench_expr before measuring")
    ap.add_argument("--count", type=int, default=20,
                    help="exact number of projection iterations")
    ap.add_argument("--mode", choices=sorted(QUERIES), default="cast",
                    help="cast = ::text casts, lit = text literals")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.simple("DROP TABLE IF EXISTS bench_expr")
    r = conn.simple("CREATE TABLE bench_expr(id INT, n INT)")
    assert tag_of(r) == "CREATE TABLE", tag_of(r)
    rows = [f"({i},{i * 7})" for i in range(args.rows)]
    for j in range(0, len(rows), 500):
        chunk = ",".join(rows[j:j + 500])
        r = conn.simple(f"INSERT INTO bench_expr VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(500, len(rows) - j)}", tag_of(r)

    query = QUERIES[args.mode]
    for _ in range(args.count):
        msgs = conn.simple(query)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} {args.mode} projections over {args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
