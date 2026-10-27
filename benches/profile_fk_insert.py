#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for bulk INSERT into a table with
an (immediate, non-deferred) FOREIGN KEY -- the ordinary, everyday shape:
a fact/detail table (`orders`, `line_items`, `events`) referencing a
dimension/parent table (`customers`, `products`), loaded in bulk by an
ETL job or batch job. FK enforcement is the default (not DEFERRABLE), so
every one of these INSERTs runs the immediate child-side FK check in
`check_fk_child_row_pv` -> `fk_parent_key_exists`
(src/exec/constraint.rs) for every row.

This target has never been profiled by a prior Bolt harness: every
existing `profile_insert*.py` harness targets a PK-less table or a
self-contained UNIQUE/PK check, never a table with an actual
`REFERENCES` constraint pointing at another table.

Launches its own `rustgres` server instance directly under valgrind
(mirrors `tests/cg_dhat_v171.py`'s self-contained pattern, not
`benches/profile.sh`'s two-terminal one), so one run of this script
produces one complete, parseable profile.

`--parent-rows` (M) sizes the referenced (parent) table; `--child-rows`
(N) is fixed per run and is how many rows the single multi-row child
INSERT carries, each referencing a valid, cyclically-chosen parent key.
Run at 3+ `--parent-rows` sizes (see `--sizes` for the convenience
driver) to see how the per-row FK check's cost scales with the size of
the table it must search.

Usage:
    cargo build
    python3 benches/profile_fk_insert.py --tool callgrind --parent-rows 1500
    python3 benches/profile_fk_insert.py --tool dhat --parent-rows 1500

    # all three sizes in one go, printing just the Ir/bytes totals:
    python3 benches/profile_fk_insert.py --tool callgrind --sizes
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
CHILD_ROWS = 1000


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


def run_one(tool, parent_rows, child_rows, keep_log=None, variant="fk"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgfkprof_")
    log_dir = tempfile.mkdtemp(prefix="rgfkprof_log_")
    log = os.path.join(log_dir, f"{tool}.out")
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
        w.simple("CREATE TABLE bench_fk_parent(id INT PRIMARY KEY, name TEXT)")
        w.simple(
            f"INSERT INTO bench_fk_parent SELECT g, 'name' || g "
            f"FROM generate_series(1, {parent_rows}) g"
        )
        ref = "REFERENCES bench_fk_parent(id)" if variant == "fk" else ""
        w.simple(
            "CREATE TABLE bench_fk_child(id INT PRIMARY KEY, "
            f"parent_id INT {ref}, val TEXT)"
        )
        # The one statement under measurement: a single multi-row INSERT
        # of `child_rows` rows, each with a valid (cyclically chosen)
        # parent_id, so every row's immediate FK check is a real,
        # successful existence lookup against the parent table.
        w.simple(
            f"INSERT INTO bench_fk_child SELECT g, ((g - 1) % {parent_rows}) + 1, "
            f"'val' || g FROM generate_series(1, {child_rows}) g"
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
                    # "totals:" has one number per "events:" column (Ir
                    # I1mr ILmr D1mr DLmr Dr Dw Bc Bcm ...); Ir is first.
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
    ap.add_argument("--parent-rows", type=int, default=1500,
                     help="rows in the referenced parent table (default 1500)")
    ap.add_argument("--child-rows", type=int, default=CHILD_ROWS,
                     help=f"rows in the single multi-row child INSERT "
                          f"(default {CHILD_ROWS})")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --parent-rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["fk", "nofk"], default="fk",
                     help="'nofk' drops the REFERENCES clause -- a control "
                          "to isolate the FK-check subtree's share of Ir by "
                          "subtraction (default 'fk')")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.parent_rows]
    for m in sizes:
        r = run_one(args.tool, m, args.child_rows,
                    keep_log=args.keep_log if not args.sizes else None,
                    variant=args.variant)
        if args.tool == "callgrind":
            print(f"parent_rows={m} child_rows={args.child_rows} Ir={r.get('Ir')}")
        else:
            # dhat log: print the summary block (total allocs/bytes).
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"parent_rows={m} child_rows={args.child_rows}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
