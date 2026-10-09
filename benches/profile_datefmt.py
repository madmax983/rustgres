#!/usr/bin/env python3
"""Fixed-iteration-count driver for a date/timestamp formatting workload.

Same rationale as profile_tochar.py: bench.py-style workloads are
time-boxed, so two profiling runs execute a different amount of work
whenever wall-clock speed jitters under valgrind, confounding a
before/after instruction/allocation comparison. This driver loads a
fixed number of rows once, then sends an exact COUNT of the same
date/timestamp-formatting SELECT over the real wire protocol.

The query formats a TIMESTAMP column two ways and a DATE column one way
with `to_char`, each call reusing its own literal format picture across
every row and every iteration -- the same shape as a real report/export
query that renders date/timestamp columns for display (e.g. an invoice
list showing "placed_at" as both a sortable ISO string and a
human-readable date). It also runs `to_date` on a text column with a
literal picture, the parsing counterpart of the same format-picture
machinery (`src/datetime.rs`'s `tokenize_format`/`parse_with_format`,
shared by `to_date`/`to_timestamp`/`to_char` on date/timestamp/text
input). Not a synthetic microbenchmark of the tokenizer alone: it drives
the real `run_statement` -> `project_row` -> `eval_func` ->
`to_date_parsed`/`format_with_pattern` call path through the wire
protocol, like every other SELECT in this file's workloads.

No prior Bolt round in this repo has profiled `src/datetime.rs`'s
format-picture tokenizer (`tokenize_format`, shared by `to_date`,
`to_timestamp`, and `to_char` on date/timestamp/text input) --
`profile_tochar.py` covers only the separate numeric-formatting engine
in `src/numfmt.rs`/`src/numfmt_tochar.rs`.

Usage:
    cargo build && ./target/debug/rustgres &                     # terminal 1
    python3 benches/profile_datefmt.py --rows 3000 --count 20   # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_datefmt.py --rows 3000 --count 20
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
    ap.add_argument("--rows", type=int, default=3000,
                     help="rows in bench_dt (default 3000)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_dt")
    r = conn.simple(
        "CREATE TABLE bench_dt(id INT, placed_at TIMESTAMP, due DATE, "
        "raw_date TEXT)")
    assert tag_of(r) == "CREATE TABLE"

    def row_vals(i):
        # Spread across ~27 years of days/seconds so no single literal
        # format picture result is cached by anything but this round's
        # fix (every row renders/parses a distinct value).
        days = 1 + (i * 37) % 9862
        secs = (i * 2654435761) % 86400
        y = 2000 + days // 365
        md = days % 365
        m = 1 + md // 31
        if m > 12:
            m = 12
        d = 1 + md % 28
        hh = secs // 3600
        mi = (secs % 3600) // 60
        ss = secs % 60
        placed_at = f"{y:04d}-{m:02d}-{d:02d} {hh:02d}:{mi:02d}:{ss:02d}"
        due = f"{y:04d}-{m:02d}-{d:02d}"
        raw_date = f"{m:02d}/{d:02d}/{y:04d}"
        return f"({i},'{placed_at}','{due}','{raw_date}')"

    for j in range(0, args.rows, 1000):
        n = min(1000, args.rows - j)
        vals = ",".join(row_vals(i) for i in range(j, j + n))
        r = conn.simple(f"INSERT INTO bench_dt VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    sql = (
        "SELECT to_char(placed_at, 'YYYY-MM-DD HH24:MI:SS'), "
        "to_char(placed_at, 'DD-MM-YYYY HH12:MI AM'), "
        "to_char(due, 'YYYY-MM-DD'), "
        "to_date(raw_date, 'MM/DD/YYYY') "
        "FROM bench_dt"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a 4-call date/timestamp "
        f"format/parse scan over {args.rows} rows",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
