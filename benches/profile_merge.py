#!/usr/bin/env python3
"""Fixed-iteration-count driver for a MERGE (upsert) workload.

Same rationale as profile_insert.py/profile_join.py: bench.py's workloads
are time-boxed, so two profiling runs execute a different amount of work
whenever wall-clock speed jitters under valgrind, confounding a
before/after instruction/allocation comparison. This driver sends an
exact COUNT of the same MERGE statement over the real wire protocol.

Workload shape: a CDC/ETL-style upsert. `bench_staging` holds a fixed
batch of incoming rows (the kind a loader would COPY in from a source
system); `bench_target` is the table being kept in sync with it via
repeated `MERGE INTO bench_target USING bench_staging ON (id) WHEN
MATCHED THEN UPDATE ... WHEN NOT MATCHED THEN INSERT ...` — the textbook
MERGE upsert, matched on a single equality predicate exactly as real
upsert statements are written. The first iteration is a cold MERGE (every
staging row is new, so every action is an INSERT); every iteration after
that is a steady-state MERGE (every staging row already exists in the
target, so every action is an UPDATE) — both phases a real sync job hits.

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_merge.py --count 15               # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_merge.py --count 15
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
    ap.add_argument("--rows", type=int, default=300,
                     help="rows in bench_staging, and in bench_target "
                          "after the first MERGE (default 300)")
    ap.add_argument("--count", type=int, default=10,
                     help="exact number of MERGE statements")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_target")
    conn.simple("DROP TABLE IF EXISTS bench_staging")
    r = conn.simple(
        "CREATE TABLE bench_target(id INT PRIMARY KEY, name TEXT, "
        "amount INT, updated_at TEXT)"
    )
    assert tag_of(r) == "CREATE TABLE"
    r = conn.simple(
        "CREATE TABLE bench_staging(id INT, name TEXT, amount INT, "
        "updated_at TEXT)"
    )
    assert tag_of(r) == "CREATE TABLE"

    rows = ",".join(
        f"({i},'cust{i}',{i * 7 % 1000},'2026-01-{(i % 28) + 1:02d}')"
        for i in range(args.rows)
    )
    r = conn.simple(f"INSERT INTO bench_staging VALUES {rows}")
    assert tag_of(r) == f"INSERT 0 {args.rows}", tag_of(r)

    sql = (
        "MERGE INTO bench_target t USING bench_staging s ON t.id = s.id "
        "WHEN MATCHED THEN UPDATE SET name = s.name, amount = s.amount, "
        "updated_at = s.updated_at "
        "WHEN NOT MATCHED THEN INSERT (id, name, amount, updated_at) "
        "VALUES (s.id, s.name, s.amount, s.updated_at)"
    )

    for i in range(args.count):
        msgs = conn.simple(sql)
        tag = tag_of(msgs)
        expect = f"MERGE {args.rows}"
        assert tag == expect, (i, tag, expect)

    print(
        f"ran {args.count} iterations of a {args.rows}-row MERGE upsert "
        f"(1 cold insert-only pass + {args.count - 1} steady-state "
        "update-only passes)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
