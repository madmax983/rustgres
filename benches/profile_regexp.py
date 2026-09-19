#!/usr/bin/env python3
"""Fixed-iteration-count driver for a regexp-heavy text workload.

Same rationale as profile_join.py/profile_fixed.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed number of text rows once, then sends an exact
COUNT of the same regexp-heavy SELECT over the real wire protocol.

The query applies `regexp_replace` (digits -> '#'), `regexp_like`
(does the row contain a run of >=3 digits) and `regexp_count` (how
many digit runs) to every row of a text table in one statement --
representative of a real bulk text-cleaning/validation query, not a
synthetic microbenchmark of the regex engine alone.

Usage:
    cargo build && ./target/debug/rustgres &                    # terminal 1
    python3 benches/profile_regexp.py --count 20                # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_regexp.py --count 20
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
                     help="rows in bench_re (default 2000)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_re")
    r = conn.simple("CREATE TABLE bench_re(id INT, s TEXT)")
    assert tag_of(r) == "CREATE TABLE"

    # Text with an embedded digit run in every row, plus a scattering of
    # rows with no digits at all -- exercises both the match and
    # no-match paths of the engine, like a real mixed-content column.
    def row_text(i):
        if i % 7 == 0:
            return f"order-ref-noref item{i} pending"
        return f"order-{i:05d}-ref item{i} qty{i % 999} pending"

    for j in range(0, args.rows, 1000):
        n = min(1000, args.rows - j)
        vals = ",".join(
            f"({i},'{row_text(i)}')" for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_re VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    sql = (
        "SELECT regexp_replace(s, '[0-9]+', '#', 'g'), "
        "regexp_like(s, '[0-9]{3,}'), "
        "regexp_count(s, '[0-9]+') "
        "FROM bench_re"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a 3-function regexp scan over "
        f"{args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
