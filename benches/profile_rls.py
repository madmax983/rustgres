#!/usr/bin/env python3
"""Fixed-iteration-count driver for multi-row INSERTs into a table with
row-level security enabled and forced.

Same rationale as profile_insert.py/profile_toast.py: bench.py's
workloads are time-boxed, so two profiling runs execute a different
amount of work whenever wall-clock speed jitters under valgrind,
confounding a before/after instruction/allocation comparison. This
driver sends an exact COUNT of multi-row VALUES INSERTs into a table
that has ROW LEVEL SECURITY enabled and FORCEd (so the owning
connection is not exempt) with one permissive tenant-isolation policy —
the standard multi-tenant SaaS shape (every row carries a tenant_id,
every statement runs WITH CHECK (tenant_id = current setting)). No
prior Bolt round in this repo has profiled the RLS path (added in
v1.95): every previous round targeted parsing, planning, execution of
plain DML, or format functions.

2026-10-20: profiled with this harness (10,000 rows, 20x500-row
INSERTs). The whole RLS WITH CHECK path (Q construction,
role_closure, the policy-expression eval) came to 1.42% of total Ir —
below this agent's 5%-of-profile floor, so no fix was shipped against
this workload. See the "Bolt negative result" issue filed the same day
for the full profile and the falsified hypothesis (a redundant
rls_applies() double-call in src/exec/rls.rs, also below floor on its
own). Kept here so a future round can re-run it against a different
RLS shape (more policies, deeper role hierarchies, SELECT-heavy
workloads) without re-deriving the harness.

Usage:
    cargo build && ./target/debug/rustgres &                   # terminal 1
    python3 benches/profile_rls.py --count 20                   # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_rls.py --count 20
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
    ap.add_argument("--rows", type=int, default=500,
                     help="rows per multi-VALUES INSERT (default 500)")
    ap.add_argument("--count", type=int, default=20,
                     help="exact number of INSERT statements")
    ap.add_argument("--timeout", type=float, default=600.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_rls")
    r = conn.simple(
        "CREATE TABLE bench_rls(id INT, tenant_id INT, body TEXT)"
    )
    assert tag_of(r) == "CREATE TABLE", tag_of(r)
    r = conn.simple("ALTER TABLE bench_rls ENABLE ROW LEVEL SECURITY")
    assert tag_of(r) == "ALTER TABLE", tag_of(r)
    r = conn.simple("ALTER TABLE bench_rls FORCE ROW LEVEL SECURITY")
    assert tag_of(r) == "ALTER TABLE", tag_of(r)
    r = conn.simple(
        "CREATE POLICY tenant_isolation ON bench_rls "
        "USING (tenant_id = 1) WITH CHECK (tenant_id = 1)"
    )
    assert tag_of(r) == "CREATE POLICY", tag_of(r)

    rows = ",".join(
        f"({i},1,'row body {i}')" for i in range(args.rows)
    )
    sql = f"INSERT INTO bench_rls VALUES {rows}"

    for _ in range(args.count):
        msgs = conn.simple(sql)
        assert tag_of(msgs) == f"INSERT 0 {args.rows}", tag_of(msgs)

    print(
        f"ran {args.count} iterations of a {args.rows}-row RLS-checked "
        "multi-VALUES INSERT",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
