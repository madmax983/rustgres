#!/usr/bin/env python3
"""Fixed-iteration-count driver for a `COPY ... FROM STDIN` bulk-load
workload (CSV format).

No prior Bolt round has profiled `copy::parse_rows` (see
benches/BASELINE.md) even though bulk-loading via `COPY FROM STDIN` is
one of the most common real-world ways data enters a Postgres-wire-
compatible server (CSV exports from spreadsheets, ETL dumps, pg_dump
restores) -- a workload shape distinct from the `insert` workload's
multi-VALUES `INSERT`, which never touches `src/copy.rs` at all.

This driver builds a realistic CSV payload for a support-ticket-shaped
table (mixed short/long text fields, some requiring CSV quoting because
they contain commas/quotes/newlines, some NULL) and sends it over the
real wire protocol via the simple-query `COPY ... FROM STDIN` path
(CopyInResponse -> CopyData* -> CopyDone -> CommandComplete), the same
fixed-iteration-count approach profile_insert.py/profile_explain.py use
so two profiling runs do identical work regardless of wall-clock jitter
under valgrind.

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_copy.py --count 20                # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_copy.py --count 20
"""
import argparse
import os
import random
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, cstr, msg, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

WORDS = (
    "the quick brown fox jumps over lazy dog customer reported issue "
    "with login page after latest update please help as soon as "
    "possible thanks team billing invoice refund shipment delayed "
    "tracking number not working account locked password reset"
).split()


def rand_text(rng, n_words):
    return " ".join(rng.choice(WORDS) for _ in range(n_words))


def build_rows(rng, n):
    """Build (id, name, email, subject, body, status) tuples. ~15% of
    bodies embed a comma/quote/newline (forcing CSV quoting), ~5% of
    subjects are NULL -- the shapes a real support-ticket export has."""
    rows = []
    statuses = ["open", "closed", "pending", "escalated"]
    for i in range(n):
        name = rand_text(rng, 2).title()
        email = f"user{i}@example.com"
        if rng.random() < 0.05:
            subject = None
        else:
            subject = rand_text(rng, 4)
        body = rand_text(rng, 25)
        if rng.random() < 0.15:
            body += ', "quoted, with comma" and a "quote" inline'
        status = rng.choice(statuses)
        rows.append((i, name, email, subject, body, status))
    return rows


def csv_field(v):
    if v is None:
        return ""
    s = str(v)
    if any(c in s for c in (",", '"', "\n", "\r")):
        return '"' + s.replace('"', '""') + '"'
    return s


def rows_to_csv(rows):
    out = []
    for r in rows:
        out.append(",".join(csv_field(v) for v in r))
    return ("\n".join(out) + "\n").encode()


def copy_from(conn, sql, payload, chunk_size=65536):
    """Simple-query `COPY ... FROM STDIN`: Q -> read to CopyInResponse,
    then CopyData frames -> CopyDone, read to ReadyForQuery."""
    conn.s.sendall(msg(b"Q", cstr(sql)))
    typ, _ = conn.rd.read_msg()
    assert typ == b"G", f"expected CopyInResponse, got {typ!r}"
    for i in range(0, len(payload), chunk_size):
        chunk = payload[i:i + chunk_size]
        conn.s.sendall(msg(b"d", chunk))
    conn.s.sendall(msg(b"c", b""))
    msgs = conn.rd.read_until(b"Z")
    return tag_of(msgs)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=2000,
                     help="rows per COPY FROM (default 2000)")
    ap.add_argument("--count", type=int, default=15,
                     help="exact number of COPY FROM statements")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    rng = random.Random(args.seed)

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_tickets")
    r = conn.simple(
        "CREATE TABLE bench_tickets(id INT, name TEXT, email TEXT, "
        "subject TEXT, body TEXT, status TEXT)"
    )
    assert tag_of(r) == "CREATE TABLE"

    rows = build_rows(rng, args.rows)
    payload = rows_to_csv(rows)
    sql = "COPY bench_tickets FROM STDIN WITH (FORMAT csv)"

    for _ in range(args.count):
        tag = copy_from(conn, sql, payload)
        assert tag == f"COPY {args.rows}", tag

    print(
        f"ran {args.count} iterations of a {args.rows}-row COPY FROM STDIN "
        f"(CSV, {len(payload)} bytes/iteration)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
