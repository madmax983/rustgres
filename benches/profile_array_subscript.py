#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for v2.40's plain-array subscript
assignment (`UPDATE t SET tags[k] = v`, src/exec/funcs.rs
eval_array_subscript_assign) -- never profiled since v2.40 added the
array function surface.

Realistic shape: a "bump one element of a per-row array column" bulk
UPDATE -- e.g. a batch job marking slot k of a fixed-size tags/scores
array dirty/updated across many rows, with the subscript always
in-bounds (the overwhelmingly common case; PG's NULL-padding expansion
path for out-of-range subscripts is the rare one). Each row holds its
own ARRAY column of `--arrsize` int elements so the cost's scaling with
array size is visible.

This harness isolates the subscript-assign cost two ways:
  * `--op subscript` vs `--op plain`: `SET tags[1] = id` vs `SET id = id`
    (same row count, same table, no array touched) -- diffing the two
    isolates the array-assign-path cost from generic per-row
    UPDATE/WAL/pgstat overhead.
  * `--arrsize N` controls each row's array length, so the per-call
    cost's scaling with array size is visible.

NOTE: an earlier version of this harness also had a `--rounds N` that
repeated the measured UPDATE against the same static rows, on the
(false) assumption that an idempotent UPDATE is "exact replicated
work" the way profile_analyze.py's read-only `--rounds` is. It is not:
each UPDATE round creates new MVCC row versions (dead tuples pile up),
so round-to-round cost is NOT deterministic/replicated -- two
back-to-back `rounds=10` runs of the identical binary measured
6,421,770,716 and 6,775,697,513 Ir (5.5% apart), while single-round
runs of the identical binary agree to ~0.003%. Dropped; do not re-add
rounds-repetition for a harness whose measured statement mutates
state.

Usage:
    cargo build
    python3 benches/profile_array_subscript.py --tool callgrind --rows 2000 --arrsize 20
    python3 benches/profile_array_subscript.py --tool callgrind --rows 2000 --arrsize 20 --op plain
    python3 benches/profile_array_subscript.py --tool dhat --rows 2000 --arrsize 20

    # scan the row-count and array-size axes in one go, printing Ir:
    python3 benches/profile_array_subscript.py --tool callgrind --row-sizes
    python3 benches/profile_array_subscript.py --tool callgrind --arr-sizes
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
DEFAULT_ROW_SIZES = [500, 1000, 2000]
DEFAULT_ARR_SIZES = [5, 20, 80]


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


def arr_literal(i, arrsize):
    """A distinct int[] array for row i, arrsize elements, so no two
    rows share identical contents (real per-row data, not a repeated
    constant)."""
    return "{" + ",".join(str(i + j) for j in range(arrsize)) + "}"


def run_one(tool, rows, arrsize, op, chunk=200, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgar_")
    log_dir = tempfile.mkdtemp(prefix="rgar_log_")
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
        w.simple("CREATE TABLE t (id int, tags int[])")
        for start in range(0, rows, chunk):
            end = min(start + chunk, rows)
            vals = ",".join(
                f"({i},'{arr_literal(i, arrsize)}')" for i in range(start, end)
            )
            w.simple(f"INSERT INTO t VALUES {vals}")

        # Measured statement: one bulk UPDATE touching every row, each
        # assignment in-bounds (no dimension expansion -- the common
        # case this harness targets).
        if op == "subscript":
            w.simple("UPDATE t SET tags[1] = id")
        else:
            w.simple("UPDATE t SET id = id")
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
                     help="rows in t, each updated via one subscript assign (default 2000)")
    ap.add_argument("--arrsize", type=int, default=20,
                     help="elements per row's tags array (default 20)")
    ap.add_argument("--op", choices=["subscript", "plain"], default="subscript",
                     help="subscript = SET tags[1]=id; "
                          "plain = SET id=id (baseline, same row count, no array touch)")
    ap.add_argument("--row-sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_ROW_SIZES} in one go (fixed --arrsize)")
    ap.add_argument("--arr-sizes", action="store_true",
                     help=f"run --arrsize at {DEFAULT_ARR_SIZES} in one go (fixed --rows)")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    if args.row_sizes:
        for n in DEFAULT_ROW_SIZES:
            r = run_one(args.tool, n, args.arrsize, args.op)
            if args.tool == "callgrind":
                print(f"rows={n} arrsize={args.arrsize} op={args.op} Ir={r.get('Ir')}")
        return
    if args.arr_sizes:
        for n in DEFAULT_ARR_SIZES:
            r = run_one(args.tool, args.rows, n, args.op)
            if args.tool == "callgrind":
                print(f"rows={args.rows} arrsize={n} op={args.op} Ir={r.get('Ir')}")
        return

    r = run_one(args.tool, args.rows, args.arrsize, args.op, keep_log=args.keep_log)
    if args.tool == "callgrind":
        print(f"rows={args.rows} arrsize={args.arrsize} op={args.op} Ir={r.get('Ir')}")
    else:
        lines = [ln for ln in r["raw"].splitlines()
                 if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
        print(f"rows={args.rows} arrsize={args.arrsize} op={args.op}")
        for ln in lines[:6]:
            print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
