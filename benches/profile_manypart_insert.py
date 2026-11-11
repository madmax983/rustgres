#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for bulk INSERT into a
RANGE-partitioned table with many leaves (v2.26:
route_partition_inserts's new leaf-local-write-semantics block in
src/exec/partition.rs, which decides per leaf group whether the leaf's
generated columns / NOT NULL flags / CHECK constraints differ from the
parent's -- added so a `PARTITION OF (...)` element-list override is
honored at leaf-write time).

That block runs once per distinct leaf touched by the statement (not
once per row) and, for every leaf, builds TWO full `TableMeta::of()`
clones (leaf + target) -- 11 `Vec::clone()` calls each, including a
`Vec<(String, ColType)>` of column names -- purely to answer "does this
leaf's definition differ from the target's", even for the overwhelming
common case (a plain child with no element-list overrides) where the
answer is always no and the clones are discarded unread.

Realistic shape: a RANGE-partitioned fact/log table with many leaves
(daily/monthly partitions over a year+ of history -- a common
production pattern), bulk-loaded via one multi-row
`INSERT ... SELECT ... FROM generate_series` that spans every leaf.
Holding total row volume equal to leaf count (1 row/leaf) isolates the
per-leaf-group block's cost from the generic per-row insert path
already profiled in prior Bolt rounds.

Two variants isolate the targeted block's share by subtraction:
  - 'manypart': RANGE-partitioned, `--parts` leaves, no column
                overrides anywhere -- exercises the new block on every
                leaf group, always taking the "no differences" fast
                exit.
  - 'plain'   : identical row volume and INSERT...SELECT shape, no
                partitioning at all -- isolates routing/insert's own
                cost from the new block.

Usage:
    cargo build
    python3 benches/profile_manypart_insert.py --tool callgrind --parts 100
    python3 benches/profile_manypart_insert.py --tool callgrind --parts 100 --variant plain
    python3 benches/profile_manypart_insert.py --tool dhat --parts 100

    # several leaf counts in one go, printing just the Ir totals:
    python3 benches/profile_manypart_insert.py --tool callgrind --sizes --variant manypart
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


def run_one(tool, parts, keep_log=None, variant="manypart"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgmanypart_")
    log_dir = tempfile.mkdtemp(prefix="rgmanypart_log_")
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
        if variant in ("manypart", "setup"):
            # RANGE-partitioned by the surrogate key `a`, `--parts`
            # leaves, no element-list overrides anywhere -- the common
            # plain-child shape the v2.26 comment calls out.
            w.simple("CREATE TABLE bench_manypart(a int, b text, val text) "
                      "PARTITION BY RANGE (a)")
            for i in range(parts):
                lo = i * PART_SPAN
                hi = (i + 1) * PART_SPAN
                w.simple(
                    f"CREATE TABLE bench_manypart_p{i} PARTITION OF bench_manypart "
                    f"FOR VALUES FROM ({lo}) TO ({hi})"
                )
            # One row per leaf (`a = i*PART_SPAN`), single multi-row
            # INSERT...SELECT, so every leaf is touched exactly once --
            # isolates the per-leaf-group block's cost from per-row
            # insert cost, which scales with total row count instead.
            values = ", ".join(f"({i * PART_SPAN}, 'k{i}', 'v{i}')" for i in range(parts))
            if variant != "setup":
                w.simple(f"INSERT INTO bench_manypart (a, b, val) VALUES {values}")
        else:
            # Plain control: identical columns, identical row volume,
            # identical INSERT shape, no partitioning at all.
            w.simple("CREATE TABLE bench_manypart(a int, b text, val text)")
            values = ", ".join(f"({i * PART_SPAN}, 'k{i}', 'v{i}')" for i in range(parts))
            w.simple(f"INSERT INTO bench_manypart (a, b, val) VALUES {values}")
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
                     help="number of leaf partitions = rows inserted (default 50)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --parts at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["manypart", "plain", "setup"], default="manypart",
                     help="'manypart' RANGE-partitions into --parts leaves with no "
                          "overrides (the v2.26 target); 'plain' has no partitioning "
                          "at all; 'setup' creates the --parts leaves but skips the "
                          "INSERT, isolating DDL setup cost (default 'manypart')")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.parts]
    for n in sizes:
        r = run_one(args.tool, n,
                    keep_log=args.keep_log if not args.sizes else None,
                    variant=args.variant)
        if args.tool == "callgrind":
            print(f"parts={n} variant={args.variant} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"parts={n} variant={args.variant}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
