#!/usr/bin/env python3
"""Fixed-iteration-count driver for a JSON-extraction-heavy workload.

Same rationale as profile_regexp.py/profile_fixed.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed number of JSON document rows once, then sends an
exact COUNT of the same JSON-extraction SELECT over the real wire
protocol.

The query applies `->`, `->>`, `#>`, `#>>` -- including a chained
`->` lookup -- to every row of a JSON-document table in one
statement: representative of a real "pull a few fields out of a JSON
payload column" report query, not a synthetic microbenchmark of the
parser alone. Each document is a moderately nested order record (an
object with a nested object, a nested array of objects, and a handful
of scalar fields) -- shaped like real request/event-log JSON, not a
toy `{"a":1}`.

Usage:
    cargo build && ./target/debug/rustgres &                 # terminal 1
    python3 benches/profile_json.py --count 20               # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_json.py --count 20
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433


def row_json(i):
    # A nested order-event document: top-level scalars, a nested
    # "customer" object, and a nested "items" array of objects --
    # the shape of a real JSON payload column (order/event log),
    # not a flat toy object.
    items = ",".join(
        '{"sku":"SKU-%05d","qty":%d,"price":%d.%02d}' % (i + k, k + 1, 10 + k, i % 100)
        for k in range(3)
    )
    return (
        '{"order_id":%d,"status":"%s","customer":{"id":%d,"name":"cust-%d",'
        '"region":"%s"},"items":[%s],"total":%d.%02d}'
    ) % (
        i,
        "pending" if i % 3 == 0 else "shipped",
        i % 500,
        i % 500,
        ["east", "west", "north", "south"][i % 4],
        items,
        100 + i,
        i % 100,
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=2000,
                     help="rows in bench_json (default 2000)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_json")
    r = conn.simple("CREATE TABLE bench_json(id INT, doc JSON)")
    assert tag_of(r) == "CREATE TABLE"

    for j in range(0, args.rows, 500):
        n = min(500, args.rows - j)
        vals = ",".join(
            "(%d,'%s')" % (i, row_json(i)) for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_json VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    # A realistic field-pull report query: top-level scalar (->>),
    # one level of nesting (-> then ->>), an array element reached via
    # #>> (path), and the full #> path to the same nested field for
    # comparison -- 5 extractions per row, like a report that denormalizes
    # a handful of JSON fields into columns.
    sql = (
        "SELECT "
        "doc->>'status', "
        "doc->'customer'->>'region', "
        "doc#>>ARRAY['customer','name'], "
        "doc#>ARRAY['items','0','sku'], "
        "doc->'items'->0->>'price' "
        "FROM bench_json"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a 5-extraction JSON scan over "
        f"{args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
