#!/usr/bin/env python3
"""Fixed-iteration-count driver for an RLS-protected UPDATE/DELETE workload.

Same rationale as profile_tsearch.py/profile_jsonpath.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different amount
of work whenever wall-clock speed jitters under valgrind, confounding a
before/after instruction/allocation comparison. This driver does a fixed
amount of work instead.

Workload: the textbook multi-tenant row-level-security pattern -- a table
with a `tenant_id` column, RLS enabled, one permissive policy restricting
every command to the caller's tenant (`CREATE POLICY ... USING (tenant_id =
$N)`), queried as a non-owner, non-superuser role (`SET SESSION
AUTHORIZATION appuser`, so RLS actually applies instead of being bypassed
by ownership/superuser status). The driver repeats a fixed number of
`UPDATE ... WHERE id % 2 = 0` and `DELETE ... WHERE false` statements
(the DELETE never actually removes a row -- its purpose is to exercise the
DELETE target-collection RLS-visibility path under callgrind without
shrinking the table across iterations) against a --rows-row table.

Usage:
    cargo build && ./target/debug/rustgres &                  # terminal 1
    python3 benches/profile_rls_update.py --rows 3000 --iters 40  # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_rls_update.py --rows 3000 --iters 40 --timeout 1800

Note: benches/profile_rls.py (added later, #57) is a separate harness
covering the RLS INSERT path; this one covers UPDATE/DELETE target-
visibility filtering. Different hot path, kept as two files.
"""
import argparse
import os
import random
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
                     help="rows in bench_rls_orders (default 3000)")
    ap.add_argument("--iters", type=int, default=40,
                     help="exact number of UPDATE+DELETE statement pairs")
    ap.add_argument("--timeout", type=float, default=1800.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    rng = random.Random(42)

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)

    conn.simple("DROP TABLE IF EXISTS bench_rls_orders")
    conn.simple("DROP USER IF EXISTS bench_rls_appuser")
    r = conn.simple(
        "CREATE TABLE bench_rls_orders("
        "id INT, tenant_id INT, status TEXT, amount INT)"
    )
    assert tag_of(r) == "CREATE TABLE", r

    # Seed phase: --rows rows across 50 tenants, mixed status, as the
    # owner (superuser postgres) so RLS has no bearing on the inserts.
    tenants = 50
    batch = 500
    for j in range(0, args.rows, batch):
        n = min(batch, args.rows - j)
        vals = ",".join(
            "({0},{1},'{2}',{3})".format(
                i, (i % tenants) + 1,
                "pending" if rng.random() < 0.7 else "closed",
                rng.randint(1, 10_000),
            )
            for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_rls_orders VALUES {vals}")
        assert tag_of(r).startswith("INSERT"), r

    r = conn.simple("CREATE USER bench_rls_appuser LOGIN")
    assert tag_of(r) == "CREATE ROLE", r
    r = conn.simple("GRANT ALL ON bench_rls_orders TO bench_rls_appuser")
    assert tag_of(r) == "GRANT", r
    r = conn.simple(
        "ALTER TABLE bench_rls_orders ENABLE ROW LEVEL SECURITY"
    )
    assert tag_of(r) == "ALTER TABLE", r
    # One permissive, ALL-commands policy for tenant 1 -- the common
    # "tenant isolation" pattern. Every row in the table is a visibility
    # candidate (the UPDATE/DELETE WHERE clauses below do not filter on
    # tenant_id), so every UPDATE/DELETE target row pays the per-row RLS
    # check; only the ~1/50 of rows with tenant_id = 1 come out visible.
    r = conn.simple(
        "CREATE POLICY tenant_isolation ON bench_rls_orders "
        "USING (tenant_id = 1) WITH CHECK (tenant_id = 1)"
    )
    assert tag_of(r) == "CREATE POLICY", r

    r = conn.simple("SET SESSION AUTHORIZATION bench_rls_appuser")
    assert tag_of(r) == "SET", r

    # UPDATE/DELETE phase: --iters fixed repetitions. UPDATE touches the
    # even-id half of the table every time (amount changes but id/status
    # don't, so the WHERE-matched row set is identical on every
    # iteration); DELETE's WHERE is always false so it never shrinks the
    # table, while still walking every row through the RLS
    # target-visibility check (dml.rs's DELETE path).
    for _ in range(args.iters):
        r = conn.simple(
            "UPDATE bench_rls_orders SET amount = amount + 1 "
            "WHERE id % 2 = 0"
        )
        assert tag_of(r).startswith("UPDATE"), r
        r = conn.simple(
            "DELETE FROM bench_rls_orders WHERE status = 'nonexistent'"
        )
        assert tag_of(r).startswith("DELETE"), r

    print(f"rls workload done: {args.rows} rows, {args.iters} "
          f"UPDATE+DELETE iterations")


if __name__ == "__main__":
    main()
