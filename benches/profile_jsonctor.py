#!/usr/bin/env python3
"""Fixed-iteration-count driver for the v1.90 SQL/JSON constructors
(JSON_OBJECT/JSON_ARRAY) on their default (no RETURNING) path.

Same rationale as profile_jsonpath.py/profile_regexp.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed number of rows of ordinary relational columns
once, then sends an exact COUNT of a JSON_OBJECT row-serialization
query over the real wire protocol.

`SELECT JSON_OBJECT('id': id, ...) FROM orders` is the textbook
SQL/JSON use case highlighted in the PG19 docs: building a JSON API
response directly from relational columns instead of serializing rows
in application code -- not a synthetic microbenchmark of the
constructor internals alone. None of v1.90 (JSON_OBJECT, JSON_ARRAY,
JSON_ARRAYAGG, JSON_OBJECTAGG, JSON_SCALAR, JSON_SERIALIZE, IS JSON)
has been profiled by any prior Bolt round.

Usage:
    cargo build && ./target/debug/rustgres &                    # terminal 1
    python3 benches/profile_jsonctor.py --count 20               # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_jsonctor.py --count 20
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
                     help="rows in bench_jsonctor (default 2000)")
    ap.add_argument("--count", type=int, default=10,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=1800.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_jsonctor")
    r = conn.simple(
        "CREATE TABLE bench_jsonctor("
        "id INT, name TEXT, status TEXT, total NUMERIC(10,2), qty INT)"
    )
    assert tag_of(r) == "CREATE TABLE"

    statuses = ["pending", "shipped", "cancelled"]
    for j in range(0, args.rows, 500):
        n = min(500, args.rows - j)
        vals = ",".join(
            "({0},'cust-{0}','{1}',{2}.{3:02d},{4})".format(
                i, statuses[i % 3], 10 + i % 900, i % 100, 1 + i % 9
            )
            for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_jsonctor VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    # Default RETURNING (json/text) -- the common case: no caller here
    # ever asks for jsonb back, which is exactly the path under test.
    sql = (
        "SELECT JSON_OBJECT("
        "'id': id, 'name': name, 'status': status, "
        "'total': total, 'qty': qty) "
        "FROM bench_jsonctor"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a 5-key JSON_OBJECT row "
        f"serialization over {args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
