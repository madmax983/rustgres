#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for WITHOUT OVERLAPS exclusion
enforcement (v2.31, src/exec/indexes.rs check_without_overlaps_batch /
check_one_wo_def) on INSERTs into a temporal PRIMARY KEY table --
never profiled since v2.31 added it.

Realistic shape: a temporal-validity table (the textbook WITHOUT
OVERLAPS use case -- room bookings, price history, entitlement
periods: `PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)`) that already
holds `--rows` settled rows, then a batch of `--inserts` further
single-row INSERTs each on a new scalar key (so no actual conflict
fires; this is the common steady-state append workload, not the error
path). Every one of those INSERTs runs
`check_without_overlaps_batch`, which collects every visible existing
row's cells into a fresh `Vec<Vec<Value>>` (one deep clone of the
whole row per existing row, every single INSERT) before the pairwise
scalar-equality + range-overlap scan -- cost intended to be driven by
the comparison scan, not by how that scan's inputs get collected.

Usage:
    cargo build
    python3 benches/profile_wo_insert.py --tool callgrind --rows 2000 --inserts 50
    python3 benches/profile_wo_insert.py --tool dhat --rows 2000 --inserts 50

    # several existing-table sizes in one go, printing just the Ir totals:
    python3 benches/profile_wo_insert.py --tool callgrind --sizes
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


def run_one(tool, rows, inserts, chunk=200, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgwo_")
    log_dir = tempfile.mkdtemp(prefix="rgwo_log_")
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
        # Temporal PK: scalar `id` (int4range, one point per row) +
        # WITHOUT OVERLAPS `valid_at` (daterange) -- the textbook
        # booking/price-history/entitlement-period shape.
        w.simple(
            "CREATE TABLE wo_t(id int4range, valid_at daterange, "
            "PRIMARY KEY (id, valid_at WITHOUT OVERLAPS))"
        )
        # Pre-populate with `rows` settled, non-conflicting rows: each
        # on its own scalar id, so setup itself never hits the O(n)
        # conflict scan's full existing-row cost (existing is empty or
        # tiny per INSERT during setup; this is just building up the
        # steady-state table we then measure INSERTs against).
        for start in range(0, rows, chunk):
            end = min(start + chunk, rows)
            vals = ",".join(
                f"('[{i},{i + 1})', '[2020-01-01,2020-01-02)')" for i in range(start, end)
            )
            w.simple(f"INSERT INTO wo_t VALUES {vals}")

        # Fixed-iteration single-row INSERTs, each on a brand-new
        # scalar id -- the common steady-state append workload (not
        # the 23P01 error path). Every one of these runs
        # check_without_overlaps_batch against the full `rows`-row
        # existing set.
        for i in range(inserts):
            nid = rows + i
            w.simple(
                f"INSERT INTO wo_t VALUES ('[{nid},{nid + 1})', "
                "'[2020-01-01,2020-01-02)')"
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
    ap.add_argument("--rows", type=int, default=2000,
                     help="pre-existing settled rows in wo_t (default 2000)")
    ap.add_argument("--inserts", type=int, default=50,
                     help="further single-row INSERTs measured (default 50)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.rows]
    for n in sizes:
        r = run_one(args.tool, n, args.inserts,
                     keep_log=args.keep_log if not args.sizes else None)
        if args.tool == "callgrind":
            print(f"rows={n} inserts={args.inserts} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"rows={n} inserts={args.inserts}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
