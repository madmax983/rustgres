#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for plan-time partition pruning
(v2.32, src/exec/prune.rs) on EXPLAINs against a RANGE-partitioned
table with many leaves.

Realistic shape: a RANGE-partitioned fact/log table with many leaves
(daily/monthly partitions over a year+ of history -- the same
production pattern as benches/profile_manypart_insert.py), then a
batch of point-lookup `EXPLAIN (COSTS OFF) SELECT * FROM bench_prune
WHERE a = N` statements spread across the leaves, driven over the
real wire protocol. `pruned_children` (prune.rs:2409) is only reached
via `sje::plan_from_item` -> `plan_partition_append` ->
`collect_pruned_leaves`, gated on PG-text mode (`pg: bool` at
explain_nodes.rs:2541, true only for `EXPLAIN (COSTS OFF)` --
confirmed empirically: plain `EXPLAIN` renders this engine's own
native plan format and never calls `plan_partition_append` at all --
`EXPLAIN SELECT * FROM t1 WHERE a = 5` prints an undifferentiated
"Seq Scan on t1", while `EXPLAIN (COSTS OFF) ...` prints the pruned
leaf, "Seq Scan on t1_p0 t1"). Not plain SELECT execution either --
same EXPLAIN-only trap as v2.30's `has_family`, issue #79. For a
RANGE table, `pruned_children` loops
over every non-default child calling `bound_may_must`, which is itself
allocation-heavy (`or_groups`, per-part `Region`/`Vec<Option<Region>>`,
`pins: Vec<_>`) -- a per-EXPLAIN cost that is O(leaf count) regardless
of how many leaves the WHERE could actually reach.

Two variants isolate pruning's share by subtraction:
  - 'prune' : RANGE-partitioned, `--parts` leaves, `--queries` point
              lookups `WHERE a = <value in exactly one leaf>` -- the
              common single-column-equality case PG's real planner
              prunes to one leaf via binary search.
  - 'plain' : identical schema, same row volume, same query shape,
              but queried against a non-partitioned table with the
              same data -- isolates pruning's own cost from the
              generic per-query scan/exec cost.

Usage:
    cargo build
    python3 benches/profile_prune_select.py --tool callgrind --parts 100 --queries 200
    python3 benches/profile_prune_select.py --tool callgrind --parts 100 --queries 200 --variant plain
    python3 benches/profile_prune_select.py --tool dhat --parts 100 --queries 200

    # several leaf counts in one go, printing just the Ir totals:
    python3 benches/profile_prune_select.py --tool callgrind --sizes --variant prune
"""
import argparse
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
DEFAULT_SIZES = [25, 50, 100]
PART_SPAN = 1000


def find_vg():
    vg = shutil.which("valgrind")
    if not vg:
        print("valgrind not found on PATH", file=sys.stderr)
        sys.exit(1)
    return vg


def free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class Wire:
    """Minimal simple-query client (startup + Query + sync wait)."""

    def __init__(self, port, timeout=600):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = self._msg()
            if t == b"Z":
                break

    def _rd(self, n):
        d = b""
        while len(d) < n:
            c = self.s.recv(n - len(d))
            if not c:
                raise RuntimeError("connection closed")
            d += c
        return d

    def _msg(self):
        t = self._rd(1)
        ln = struct.unpack("!i", self._rd(4))[0]
        return t, self._rd(ln - 4)

    def simple(self, q):
        qb = q.encode()
        self.s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
        msgs = []
        while True:
            t, p = self._msg()
            msgs.append((t, p))
            if t == b"Z":
                return msgs
            if t == b"E":
                raise RuntimeError(p)

    def close(self):
        self.s.close()


def run_one(tool, parts, queries, keep_log=None, variant="prune"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgprune_")
    log_dir = tempfile.mkdtemp(prefix="rgprune_log_")
    # NOTE: --log-file must not collide with --callgrind-out-file /
    # --dhat-out-file below (both under log_dir) -- a collision splices
    # valgrind's own summary text into the profile data. See
    # benches/BASELINE.md, 2026-11-09 route_partition_inserts entry.
    log = os.path.join(log_dir, f"{tool}.log")
    args = [vg, f"--tool={tool}", f"--log-file={log}"]
    if tool == "callgrind":
        args += [
            "--callgrind-out-file=" + os.path.join(log_dir, "callgrind.out"),
            "--collect-jumps=yes",
            "--cache-sim=yes",
            "--branch-sim=yes",
        ]
    elif tool == "dhat":
        args += ["--dhat-out-file=" + os.path.join(log_dir, "dhat.out")]
    proc = subprocess.Popen(
        args + [BIN, "--data-dir", data_dir, "--port", str(port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", port), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            raise RuntimeError("server did not start")

        w = Wire(port, timeout=900)
        if variant == "prune":
            # RANGE-partitioned by the surrogate key `a`, `--parts`
            # leaves, no element-list overrides.
            w.simple("CREATE TABLE bench_prune(a int, b text, val text) "
                      "PARTITION BY RANGE (a)")
            for i in range(parts):
                lo = i * PART_SPAN
                hi = (i + 1) * PART_SPAN
                w.simple(
                    f"CREATE TABLE bench_prune_p{i} PARTITION OF bench_prune "
                    f"FOR VALUES FROM ({lo}) TO ({hi})"
                )
            values = ", ".join(f"({i * PART_SPAN}, 'k{i}', 'v{i}')" for i in range(parts))
            w.simple(f"INSERT INTO bench_prune (a, b, val) VALUES {values}")
        else:
            # Plain control: identical columns, identical row volume,
            # no partitioning at all.
            w.simple("CREATE TABLE bench_prune(a int, b text, val text)")
            values = ", ".join(f"({i * PART_SPAN}, 'k{i}', 'v{i}')" for i in range(parts))
            w.simple(f"INSERT INTO bench_prune (a, b, val) VALUES {values}")

        # Fixed-iteration point-lookup EXPLAINs, each landing in
        # exactly one leaf (a single-column equality -- the common
        # case PG's real planner prunes via binary search). EXPLAIN,
        # not plain SELECT: plan_partition_append/pruned_children only
        # run in PG-text (EXPLAIN) mode -- see module docstring.
        for i in range(queries):
            target = (i % parts) * PART_SPAN
            w.simple(f"EXPLAIN (COSTS OFF) SELECT * FROM bench_prune WHERE a = {target}")
        w.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except Exception:
            proc.kill()
        shutil.rmtree(data_dir, ignore_errors=True)

    result = {}
    if tool == "callgrind":
        cg_out = os.path.join(log_dir, "callgrind.out")
        with open(cg_out) as f:
            for line in f:
                if line.startswith("totals:"):
                    result["Ir"] = int(line.split(":")[1].strip().split()[0])
        result["path"] = cg_out
    elif tool == "dhat":
        result["path"] = os.path.join(log_dir, "dhat.out")
        with open(result["path"]) as f:
            txt = f.read()
        result["raw"] = txt
    if keep_log:
        shutil.copy(result["path"], keep_log)
        result["kept"] = keep_log
    else:
        shutil.rmtree(log_dir, ignore_errors=True)
    return result


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                  formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tool", choices=["callgrind", "dhat"], default="callgrind")
    ap.add_argument("--parts", type=int, default=50,
                     help="number of leaf partitions (default 50)")
    ap.add_argument("--queries", type=int, default=200,
                     help="number of point-lookup SELECTs (default 200)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --parts at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["prune", "plain"], default="prune",
                     help="'prune' RANGE-partitions into --parts leaves (the v2.32 "
                          "target); 'plain' has no partitioning at all (default 'prune')")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.parts]
    for n in sizes:
        r = run_one(args.tool, n, args.queries,
                    keep_log=args.keep_log if not args.sizes else None,
                    variant=args.variant)
        if args.tool == "callgrind":
            print(f"parts={n} queries={args.queries} variant={args.variant} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"parts={n} queries={args.queries} variant={args.variant}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
