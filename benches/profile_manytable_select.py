#!/usr/bin/env python3
"""Fixed-iteration-count driver for repeated EXPLAIN SELECTs against one
table in a schema with many other, unrelated tables.

Rationale: v2.30 (extended-statistics planner consumption) added
`has_family`/`family_tables` (src/exec/extstats.rs) to the scan-planning
path every EXPLAIN runs for every scan node, to decide whether a scan
must cover a whole inheritance/partition family
(src/exec/sje.rs:1589/1591, src/exec/explain_nodes.rs:2519/2520,
src/exec/mvest.rs's `has_family`). This path is EXPLAIN-only: a plain
(non-EXPLAIN) SELECT is executed by src/exec/from.rs/join.rs directly and
never calls it -- confirmed by profiling a plain-SELECT version of this
same workload under callgrind, where none of src/exec/{sje,mvest,
extstats}.rs appear at all.

`family_tables` has no index to answer "does any other table inherit
from this one" except scanning every entry of `db.tables` (a
HashMap<String, Vec<Table>>) -- so its cost is O(total tables in the
database), paid on every scan node of every EXPLAIN, even for a plain,
non-partitioned, non-inherited table (the overwhelmingly common case).
No prior Bolt round has profiled a many-table schema; every earlier
harness in this directory uses 1-3 tables, where an O(table count)
per-EXPLAIN cost is invisible. A real application database commonly has
on the order of a few hundred tables (ORMs, migrations, multi-tenant
schemas); this harness models that by creating `--tables` unrelated,
empty decoy tables alongside the one actually queried, then sends an
exact COUNT of single-row point-lookup `EXPLAIN SELECT` statements
against it. EXPLAIN is a real, public wire-protocol entry point (used by
query-plan-caching ORMs, auto_explain-style tooling, and interactive
DBA use), not a synthetic microbenchmark of the function under test.

The iteration count is intentionally large relative to the one-time
`--tables` setup (which itself costs some instructions, from the
CREATE TABLE statements) so that the per-EXPLAIN cost dominates the
total and isn't diluted by setup noise across before/after runs.

Same fixed-iteration-count rationale as profile_idxscan.py/profile_scan.py:
bench.py's workloads are time-boxed, which would make two profiling runs
execute a different amount of work whenever wall-clock speed jitters
under valgrind.

Usage:
    cargo build && ./target/debug/rustgres &                          # terminal 1
    python3 benches/profile_manytable_select.py --tables 300 --count 1500  # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_manytable_select.py --tables 300 --count 1500
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
    ap.add_argument("--tables", type=int, default=300,
                     help="unrelated decoy tables created alongside the "
                          "queried table, to model a real multi-table schema")
    ap.add_argument("--rows", type=int, default=2000,
                     help="rows to load into bench_many before measuring")
    ap.add_argument("--count", type=int, default=1500,
                     help="exact number of EXPLAIN point-lookup iterations")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "default 30s from bench.py's Conn is too short)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)

    conn.simple("DROP TABLE IF EXISTS bench_many")
    for i in range(args.tables):
        conn.simple(f"DROP TABLE IF EXISTS bench_decoy_{i}")

    for i in range(args.tables):
        r = conn.simple(f"CREATE TABLE bench_decoy_{i}(id INT, v TEXT)")
        assert tag_of(r) == "CREATE TABLE"

    r = conn.simple("CREATE TABLE bench_many(id INT, grp INT, name TEXT)")
    assert tag_of(r) == "CREATE TABLE"
    rows = []
    for i in range(args.rows):
        rows.append(f"({i},{i % 100},'name{i}')")
    for j in range(0, len(rows), 1000):
        chunk = ",".join(rows[j:j + 1000])
        r = conn.simple(f"INSERT INTO bench_many VALUES {chunk}")
        assert tag_of(r) == f"INSERT 0 {min(1000, len(rows) - j)}", tag_of(r)

    # Deterministic sequence of point lookups, walking evenly across the
    # key space so repeated runs do identical work.
    step = max(args.rows // max(args.count, 1), 1)
    for k in range(args.count):
        target = (k * step) % args.rows
        msgs = conn.simple(f"EXPLAIN SELECT * FROM bench_many WHERE id = {target}")
        assert tag_of(msgs) == "EXPLAIN", tag_of(msgs)
    print(f"ran {args.count} EXPLAIN point-lookup SELECTs against bench_many "
          f"({args.rows} rows) alongside {args.tables} decoy tables",
          file=sys.stderr)


if __name__ == "__main__":
    main()
