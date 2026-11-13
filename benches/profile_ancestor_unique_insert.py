#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for bulk INSERT into a
RANGE-partitioned table that carries a standalone UNIQUE INDEX on its
parent (v2.25: `ancestor_unique_indexes`/`check_leaf_ancestor_uniques`
in src/exec/indexes.rs -- the new per-leaf consultative enforcement for
a partitioned unique index, since PG19 gives every leaf a physical
child index but this engine keeps no such child-index objects).

v2.25 landed after the last Bolt round (2026-11-09's
route_partition_inserts fix) and has never been profiled. The realistic
shape is a table partitioned by an internal/surrogate key (an id or a
date range) that also carries a separate business-key UNIQUE
constraint declared once on the parent -- e.g. `orders` partitioned by
`order_id` range, with a standalone unique index on `external_ref` --
exactly `idxpart`/`create unique index on idxpart (a)` from
src/exec/test_mods/v225_tests.rs, at bulk-insert volume.

Three variants isolate the new code's share by subtraction:
  - 'ancestor' : RANGE-partitioned, 4 leaves, UNIQUE INDEX on the
                 parent over a non-partition-key column -- exercises
                 ancestor_unique_indexes/check_leaf_ancestor_uniques.
  - 'nounique' : identical partitioning, no unique index anywhere --
                 same INSERT...SELECT shape and row volume, isolates
                 v2.25's any_unique probe overhead with nothing to
                 enforce.
  - 'plain'    : no partitioning at all -- isolates partition routing
                 itself (already profiled in prior Bolt rounds) from
                 the new unique-enforcement work.

Usage:
    cargo build
    python3 benches/profile_ancestor_unique_insert.py --tool callgrind --rows 2000 --variant ancestor
    python3 benches/profile_ancestor_unique_insert.py --tool callgrind --rows 2000 --variant nounique
    python3 benches/profile_ancestor_unique_insert.py --tool dhat --rows 2000 --variant ancestor

    # all three sizes in one go, printing just the Ir/bytes totals:
    python3 benches/profile_ancestor_unique_insert.py --tool callgrind --sizes --variant ancestor
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
DEFAULT_SIZES = [500, 1000, 2000]
NPART = 4
PART_SPAN = 100_000


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


def run_one(tool, rows, keep_log=None, variant="ancestor"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgancprof_")
    log_dir = tempfile.mkdtemp(prefix="rgancprof_log_")
    # NOTE: --log-file must not collide with --callgrind-out-file /
    # --dhat-out-file below (both under log_dir) -- a collision
    # splices valgrind's own summary text into the profile data,
    # which makes callgrind_annotate choke ("too many event counts")
    # and the dhat file invalid JSON. See benches/BASELINE.md,
    # 2026-11-09 route_partition_inserts entry.
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
        if variant in ("ancestor", "nounique"):
            # RANGE-partitioned by the surrogate key `a` -- the common
            # real-world shape for a partitioned fact/log table.
            w.simple("CREATE TABLE bench_ancpart(a int, b text, val text) "
                      "PARTITION BY RANGE (a)")
            for i in range(NPART):
                lo = i * PART_SPAN
                hi = (i + 1) * PART_SPAN
                w.simple(
                    f"CREATE TABLE bench_ancpart_p{i} PARTITION OF bench_ancpart "
                    f"FOR VALUES FROM ({lo}) TO ({hi})"
                )
            if variant == "ancestor":
                # A business-key UNIQUE constraint declared once on the
                # parent (e.g. an external reference column) -- PG19
                # gives every leaf a physical child index; this is the
                # v2.25 consultative-enforcement target.
                w.simple("CREATE UNIQUE INDEX ON bench_ancpart (b)")
            w.simple(
                f"INSERT INTO bench_ancpart SELECT g, 'k' || g, 'v' || g "
                f"FROM generate_series(1, {rows}) g"
            )
        else:
            # Plain control: identical columns, identical row volume,
            # identical INSERT ... SELECT shape, no partitioning at all.
            w.simple("CREATE TABLE bench_ancpart(a int, b text, val text)")
            w.simple(
                f"INSERT INTO bench_ancpart SELECT g, 'k' || g, 'v' || g "
                f"FROM generate_series(1, {rows}) g"
            )
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
    ap.add_argument("--rows", type=int, default=1000,
                     help="rows in the single multi-row INSERT (default 1000)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["ancestor", "nounique", "plain"],
                     default="ancestor",
                     help="'ancestor' adds a standalone UNIQUE INDEX on "
                          "the partitioned parent (the v2.25 target); "
                          "'nounique' is identically partitioned with no "
                          "unique index; 'plain' has no partitioning at "
                          "all (default 'ancestor')")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.rows]
    for n in sizes:
        r = run_one(args.tool, n,
                    keep_log=args.keep_log if not args.sizes else None,
                    variant=args.variant)
        if args.tool == "callgrind":
            print(f"rows={n} variant={args.variant} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"rows={n} variant={args.variant}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
