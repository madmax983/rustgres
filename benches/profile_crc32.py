#!/usr/bin/env python3
"""Fixed-iteration-count driver for a bulk crc32c()/crc32() checksum scan.

Same rationale as profile_regexp.py/profile_toast.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver loads a fixed number of document-sized TEXT rows once, then
sends an exact COUNT of a `crc32c(body)` scan over every row via the
real wire protocol.

`crc32c()`/`crc32()` (added for PG 18 parity) are the checksum
functions a real client calls to verify or deduplicate bulk content
(log lines, document bodies, blob chunks) without pulling the whole
value back over the wire -- compute the checksum in the database, send
back 4 bytes. No prior Bolt round in this repo has profiled
`exec::funcs::crc32c`/`crc32`.

Usage:
    cargo build && ./target/debug/rustgres &                    # terminal 1
    python3 benches/profile_crc32.py --count 20                 # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_crc32.py --count 20
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

# A fixed corpus of prose sentences, cycled to build each row's body --
# same shape as profile_toast.py's documents (real redundancy, not a
# single repeated byte), since crc32c/crc32 are exposed on exactly this
# kind of wide TEXT/bytea column.
_SENTENCES = [
    "The quick brown fox jumps over the lazy dog near the river bank.",
    "Database systems must balance durability, consistency, and speed.",
    "Every checksum call walks the full byte span of its argument.",
    "Content hashes let clients dedup blobs without re-sending them.",
    "A table-driven CRC trades a small lookup table for fewer branches.",
    "Storage engines verify checksums before trusting a page is intact.",
    "Query planners never see the checksum functions internal cost.",
    "Logs, articles, and JSON blobs are the common wide column shapes.",
    "Repeated substrings inside prose give a real workload real content.",
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
    ap.add_argument("--rows", type=int, default=1500,
                     help="rows in bench_crc (default 1500)")
    ap.add_argument("--body-bytes", type=int, default=4096,
                     help="approx bytes in the TEXT column per row "
                          "(default 4096)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of query iterations (each touches "
                          "every row)")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_crc")
    r = conn.simple("CREATE TABLE bench_crc(id INT, body TEXT)")
    assert tag_of(r) == "CREATE TABLE"

    for j in range(0, args.rows, 200):
        n = min(200, args.rows - j)
        vals = ",".join(
            f"({i},'{make_body(i, args.body_bytes)}')" for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_crc VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    sql = "SELECT crc32c(body), crc32(body) FROM bench_crc"

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"SELECT {args.rows}", tag_of(msgs)
    print(
        f"ran {args.count} iterations of a crc32c()/crc32() scan over "
        f"{args.rows} rows (~{args.body_bytes} bytes each)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
