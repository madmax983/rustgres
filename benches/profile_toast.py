#!/usr/bin/env python3
"""Fixed-iteration-count driver for wide-row INSERTs that trigger TOAST.

Same rationale as profile_insert.py/profile_join.py/profile_scan.py:
bench.py's workloads are time-boxed, so two profiling runs execute a
different amount of work whenever wall-clock speed jitters under
valgrind, confounding a before/after instruction/allocation comparison.
This driver sends an exact COUNT of single-row INSERTs of a wide TEXT
column (a document/log-line/JSON-blob shape: one write, one wide
column, real prose-like redundancy) over the real wire protocol.

No prior Bolt round in this repo has profiled the TOAST path (added in
v0.37) — every previous round targeted the tokenizer/parser, row
send/project, or join/order-by comparisons. A single-row INSERT of a
document-sized TEXT value is a realistic shape (CMS body, log line,
serialized JSON) that pushes rows above `TOAST_TUPLE_THRESHOLD` (2037
bytes) and so drives the PGLZ compressor added for wide-row storage.

Usage:
    cargo build && ./target/debug/rustgres &                    # terminal 1
    python3 benches/profile_toast.py --count 300                # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=dhat ./target/debug/rustgres &
    python3 benches/profile_toast.py --count 300
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

# A fixed corpus of prose sentences, cycled to build each row's body.
# Real English prose (not a repeated single string) so the PGLZ
# compressor sees realistic 3-byte-match redundancy, not a pathological
# best case.
_SENTENCES = [
    "The quick brown fox jumps over the lazy dog near the river bank.",
    "Database systems must balance durability, consistency, and speed.",
    "Every wide column pushes the row closer to the TOAST threshold.",
    "Compression trades CPU time now for less disk time later on.",
    "A write amplifies once the planner decides the value is large.",
    "Storage engines chunk oversized values into a side relation.",
    "Query planners estimate row width before choosing a join order.",
    "Logs, articles, and JSON blobs are the common wide column shapes.",
    "Repeated substrings inside prose give a real compressor real work.",
    "Benchmarks should mirror the shapes a real client sends in.",
]


def make_body(row_id: int, target_len: int) -> str:
    """Deterministic ~target_len-byte prose body for row row_id."""
    parts = [f"document {row_id} begins here."]
    i = row_id
    total = len(parts[0])
    while total < target_len:
        s = _SENTENCES[i % len(_SENTENCES)]
        parts.append(s)
        total += len(s) + 1
        i += 1
    parts.append(f"document {row_id} ends here.")
    return " ".join(parts)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--body-bytes", type=int, default=4096,
                     help="approx bytes in the TEXT column per row "
                          "(default 4096; TOAST threshold is 2037)")
    ap.add_argument("--count", type=int, default=300,
                     help="exact number of single-row INSERT statements")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_toast")
    r = conn.simple("CREATE TABLE bench_toast(id INT, body TEXT)")
    assert tag_of(r) == "CREATE TABLE"

    for i in range(args.count):
        body = make_body(i, args.body_bytes)
        sql = f"INSERT INTO bench_toast VALUES ({i}, '{body}')"
        msgs = conn.simple(sql)
        assert tag_of(msgs) == "INSERT 0 1", tag_of(msgs)

    print(
        f"ran {args.count} iterations of a single-row ~{args.body_bytes}-byte "
        "TEXT INSERT (TOAST path)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
