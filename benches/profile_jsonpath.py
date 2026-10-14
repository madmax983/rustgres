#!/usr/bin/env python3
"""Fixed-iteration-count driver for a jsonb_path_*-heavy workload.

Same rationale as profile_regexp.py/profile_join.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed number of jsonb rows once, then sends an exact
COUNT of the same jsonpath-heavy SELECT over the real wire protocol.

The query applies three jsonb_path_* functions (exists/match/
query_array), each with a literal jsonpath argument, to every row of a
jsonb table in one statement -- representative of a real
order/event-log filtering-and-extraction query (SQL/JSON predicates +
projections), not a synthetic microbenchmark of the jsonpath engine
alone. Documents are shaped like real order events: a top-level
status/total, a nested customer object, and a nested items array of
object -- not a toy `{"a":1}`.

Usage:
    cargo build && ./target/debug/rustgres &                    # terminal 1
    python3 benches/profile_jsonpath.py --count 20               # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_jsonpath.py --count 20
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433


def doc_json(i):
    status = ["pending", "shipped", "cancelled"][i % 3]
    region = ["west", "east", "north", "south"][i % 4]
    items = ",".join(
        '{{"sku": "SKU-{0}", "price": {1}.{2:02d}, "qty": {3}}}'.format(
            (i * 7 + k) % 500, 5 + k, (i + k) % 100, 1 + (i + k) % 9
        )
        for k in range(3)
    )
    return (
        '{{"status": "{status}", "total": {total}.{cents:02d}, '
        '"customer": {{"name": "cust-{i}", "region": "{region}"}}, '
        '"items": [{items}]}}'
    ).format(
        status=status,
        total=10 + i % 900,
        cents=i % 100,
        i=i,
        region=region,
        items=items,
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=1500,
                     help="rows in bench_jsonpath (default 1500)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=1800.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_jsonpath")
    r = conn.simple("CREATE TABLE bench_jsonpath(id INT, doc JSONB)")
    assert tag_of(r) == "CREATE TABLE"

    for j in range(0, args.rows, 500):
        n = min(500, args.rows - j)
        vals = ",".join(
            "({0},'{1}')".format(i, doc_json(i)) for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_jsonpath VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    sql = (
        "SELECT jsonb_path_exists(doc, '$.items[*] ? (@.qty > 5)'), "
        "jsonb_path_match(doc, '$.status == \"shipped\"'), "
        "jsonb_path_query_array(doc, '$.items[*].price') "
        "FROM bench_jsonpath"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a 3-function jsonb_path_* scan "
        f"over {args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
