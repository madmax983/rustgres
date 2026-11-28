#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for batched INSERT...SELECT into a
table with a `GENERATED ALWAYS AS (expr) STORED` column -- every row calls
`compute_generated_columns` (src/exec/constraint.rs) once via the write
path's "v2.03: generated columns materialize on write" step. This function
has existed since v2.03 and is on every INSERT/UPDATE/MERGE/partition-route
row path for any table with a generated column, but no prior Bolt round has
profiled it.

`--variant generated` (default) creates the table with a `GENERATED ALWAYS
AS (a + 1) STORED` column `b`, forcing one `compute_generated_columns` call
per row. `--variant plain` creates the same table shape but with `b` as an
ordinary column (explicit value, no generation expression) -- a control
that isolates `compute_generated_columns`'s share of the statement's cost
by subtraction.

Launches its own `rustgres` server instance directly under valgrind (same
self-contained pattern as `profile_serial_insert.py`).

Usage:
    cargo build
    python3 benches/profile_generated_insert.py --tool callgrind --rows 20000
    python3 benches/profile_generated_insert.py --tool dhat --rows 20000

    # both variants at one size, printing just the Ir/bytes totals:
    python3 benches/profile_generated_insert.py --tool callgrind --rows 20000 --both
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
DEFAULT_ROWS = [5000, 10000, 20000]


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

    def __init__(self, port, timeout=900):
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


def run_one(tool, rows, variant, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rggenprof_")
    log_dir = tempfile.mkdtemp(prefix="rggenprof_log_")
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
        if variant == "generated":
            w.simple(
                "CREATE TABLE bench_gen(id INT PRIMARY KEY, a INT, "
                "b INT GENERATED ALWAYS AS (a + 1) STORED)"
            )
            # b omitted on every row: each row's write path calls
            # compute_generated_columns once to materialize it.
            w.simple(
                "INSERT INTO bench_gen(id, a) SELECT g, g "
                f"FROM generate_series(1, {rows}) g"
            )
        else:
            w.simple("CREATE TABLE bench_gen(id INT PRIMARY KEY, a INT, b INT)")
            # explicit b on every row: identical shape and row count,
            # zero compute_generated_columns work (meta.gen_cols is
            # all-None, so the function's fast-path early-return fires).
            w.simple(
                "INSERT INTO bench_gen(id, a, b) SELECT g, g, g + 1 "
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
    ap.add_argument("--rows", type=int, default=20000,
                     help="rows inserted by the single statement under "
                          "measurement (default 20000)")
    ap.add_argument("--variant", choices=["generated", "plain"], default="generated",
                     help="'generated' (default) has a GENERATED ALWAYS AS "
                          "(...) STORED column so every row calls "
                          "compute_generated_columns; 'plain' is a control "
                          "with zero compute_generated_columns work")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_ROWS} in one go")
    ap.add_argument("--both", action="store_true",
                     help="run both variants at --rows in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_ROWS if args.sizes else [args.rows]
    variants = ["generated", "plain"] if args.both else [args.variant]
    single = not args.sizes and not args.both
    for n in sizes:
        for variant in variants:
            r = run_one(args.tool, n, variant,
                        keep_log=args.keep_log if single else None)
            if args.tool == "callgrind":
                print(f"variant={variant} rows={n} Ir={r.get('Ir')}")
            else:
                lines = [ln for ln in r["raw"].splitlines()
                         if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
                print(f"variant={variant} rows={n}")
                for ln in lines[:6]:
                    print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
