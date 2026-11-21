#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for v2.35's schema-qualified
type-name resolution (`Database::find_type`, src/storage.rs) on a
named-type CAST evaluated once per row -- never profiled since v2.35
added it.

Realistic shape: a composite type created in a non-public schema
(`CREATE SCHEMA s; CREATE TYPE s.pt AS (x int, y int)`) -- the textbook
"organize types under a schema" pattern v2.35 exists to support --
referenced by its BARE name in a cast repeated over every row of a
table (`SELECT ROW(x,y)::pt FROM t`), which is how PG itself expects a
same-schema/search-path reference to resolve. Confirmed end-to-end
correct (not just reachable) with a manual probe before writing this
harness: `SELECT ROW(x,y)::pt FROM t` returns the same rows as the
fully qualified `::s.pt` spelling.

Every evaluation of that cast calls `eval_cast_composite_inner` ->
`Database::find_type("pt")`. `pt` is stored under the dotted key
`"s.pt"`, so the fast exact-match branch in `find_type` misses, and
bare (undotted) names fall through to:

    let suffix = format!(".{name}");          // one allocation, EVERY call
    for (k, st) in self.types.iter() {         // O(len(self.types)), EVERY call
        if k.ends_with(&suffix) { ... }
    }

This harness isolates that per-row cost two ways:
  * `--decoys N` pads `self.types` with N additional dotted shell
    types in the same schema (`CREATE TYPE s.d0; ...`) so the O(k)
    scan has a visible k to scan -- a database with several
    schema-organized types is the realistic case this feature targets,
    not a contrived one.
  * `--rows 0` on an otherwise-identical setup isolates the one-time
    DDL cost from the per-row cast cost (same subtraction trick as
    profile_wo_insert.py / profile_pgstat_update.py): run once with
    `--rows N --cast 1` and once with `--rows N --cast 0` (plain
    `SELECT ROW(x,y) FROM t`, no cast) and diff.

Usage:
    cargo build
    python3 benches/profile_qualified_cast.py --tool callgrind --rows 2000 --decoys 50
    python3 benches/profile_qualified_cast.py --tool callgrind --rows 2000 --decoys 50 --cast 0
    python3 benches/profile_qualified_cast.py --tool dhat --rows 2000 --decoys 50

    # scan the decoy-count and row-count axes in one go, printing Ir:
    python3 benches/profile_qualified_cast.py --tool callgrind --decoy-sizes
    python3 benches/profile_qualified_cast.py --tool callgrind --row-sizes
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
DEFAULT_DECOY_SIZES = [0, 50, 200]
DEFAULT_ROW_SIZES = [500, 1000, 2000]


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


def run_one(tool, rows, decoys, cast, chunk=200, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgqc_")
    log_dir = tempfile.mkdtemp(prefix="rgqc_log_")
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
        # Schema-organized composite type -- the realistic v2.35 shape
        # (type created under a non-public schema, referenced bare).
        w.simple("CREATE SCHEMA s")
        w.simple("CREATE TYPE s.pt AS (x int, y int)")
        # Decoys: other dotted types sharing the schema, so the bare
        # "pt" lookup's O(len(self.types)) scan has a realistic k to
        # walk (a schema with several organized types, not just one).
        for i in range(0, decoys, chunk):
            end = min(i + chunk, decoys)
            stmts = ";".join(f"CREATE TYPE s.d{j}" for j in range(i, end))
            if stmts:
                w.simple(stmts)
        w.simple("CREATE TABLE t (x int, y int)")
        for start in range(0, rows, chunk):
            end = min(start + chunk, rows)
            vals = ",".join(f"({i},{i + 1})" for i in range(start, end))
            w.simple(f"INSERT INTO t VALUES {vals}")

        # Measured statement: one SELECT scanning every row, each row
        # evaluating the cast (or not, for the --cast 0 baseline).
        if cast:
            w.simple("SELECT ROW(x,y)::pt FROM t")
        else:
            w.simple("SELECT ROW(x,y) FROM t")
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
                     help="rows in t, each producing one cast eval (default 2000)")
    ap.add_argument("--decoys", type=int, default=50,
                     help="extra dotted shell types in schema s (default 50)")
    ap.add_argument("--cast", type=int, choices=[0, 1], default=1,
                     help="1 = SELECT ROW(x,y)::pt FROM t, 0 = SELECT ROW(x,y) FROM t (baseline)")
    ap.add_argument("--decoy-sizes", action="store_true",
                     help=f"run --decoys at {DEFAULT_DECOY_SIZES} in one go (fixed --rows)")
    ap.add_argument("--row-sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_ROW_SIZES} in one go (fixed --decoys)")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    if args.decoy_sizes:
        for n in DEFAULT_DECOY_SIZES:
            r = run_one(args.tool, args.rows, n, args.cast)
            if args.tool == "callgrind":
                print(f"rows={args.rows} decoys={n} cast={args.cast} Ir={r.get('Ir')}")
        return
    if args.row_sizes:
        for n in DEFAULT_ROW_SIZES:
            r = run_one(args.tool, n, args.decoys, args.cast)
            if args.tool == "callgrind":
                print(f"rows={n} decoys={args.decoys} cast={args.cast} Ir={r.get('Ir')}")
        return

    r = run_one(args.tool, args.rows, args.decoys, args.cast, keep_log=args.keep_log)
    if args.tool == "callgrind":
        print(f"rows={args.rows} decoys={args.decoys} cast={args.cast} Ir={r.get('Ir')}")
    else:
        lines = [ln for ln in r["raw"].splitlines()
                 if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
        print(f"rows={args.rows} decoys={args.decoys} cast={args.cast}")
        for ln in lines[:6]:
            print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
