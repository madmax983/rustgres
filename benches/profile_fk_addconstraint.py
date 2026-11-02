#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for ALTER TABLE ... ADD CONSTRAINT
... FOREIGN KEY on a table that already holds rows -- the ordinary
migration shape: a team adds a missing FK to an existing, already-
populated child table (`orders.customer_id -> customers.id`), which is
common enough that v2.15 added the validation this harness measures
(see BASELINE.md "v2.15" entry and src/exec/alter.rs's
`validate_fk_rows`, called from `alter_add_constraint` when the new FK
is neither NOT VALID nor NOT ENFORCED).

This path has never been profiled by a prior Bolt harness:
`profile_fk_insert.py` measures the immediate per-row FK check on
ordinary INSERTs into an *already-constrained* table; this harness
measures the one-time bulk scan ADD CONSTRAINT runs over a table's
*existing* rows when the constraint is added after the data, which is
a structurally different call path (`validate_fk_rows`, not the
INSERT/UPDATE executor).

Launches its own `rustgres` server instance directly under valgrind
(same self-contained pattern as `profile_fk_insert.py`).

`--parent-rows` (M) sizes the referenced parent table; `--child-rows`
(N) sizes the child table the single ADD CONSTRAINT statement scans.
`--variant enforced` (default) adds a plain enforced FK, which runs
the full `validate_fk_rows` scan; `--variant notvalid` adds the same
FK as NOT VALID, which skips the scan entirely -- a control to isolate
the scan's share of `Ir` by subtraction.

Usage:
    cargo build
    python3 benches/profile_fk_addconstraint.py --tool callgrind --child-rows 2000
    python3 benches/profile_fk_addconstraint.py --tool dhat --child-rows 2000

    # all three sizes in one go, printing just the Ir/bytes totals:
    python3 benches/profile_fk_addconstraint.py --tool callgrind --sizes
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
PARENT_ROWS = 1500


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


def run_one(tool, parent_rows, child_rows, keep_log=None, variant="enforced"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgfkacprof_")
    log_dir = tempfile.mkdtemp(prefix="rgfkacprof_log_")
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
        # Child table created WITHOUT the FK, then populated -- the
        # "data already exists, FK is added later" migration shape.
        w.simple("CREATE TABLE bench_fk_child(id INT PRIMARY KEY, parent_id INT, val TEXT)")
        w.simple(
            f"INSERT INTO bench_fk_child SELECT g, ((g - 1) % {parent_rows}) + 1, "
            f"'val' || g FROM generate_series(1, {child_rows}) g"
        )
        # The one statement under measurement: ADD CONSTRAINT scans every
        # existing child row against the parent (enforced) or skips the
        # scan entirely (notvalid control).
        not_valid = "NOT VALID" if variant == "notvalid" else ""
        w.simple(
            "ALTER TABLE bench_fk_child ADD CONSTRAINT bench_fk "
            f"FOREIGN KEY (parent_id) REFERENCES bench_fk_parent(id) {not_valid}"
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
    ap.add_argument("--parent-rows", type=int, default=PARENT_ROWS,
                     help=f"rows in the referenced parent table (default {PARENT_ROWS})")
    ap.add_argument("--child-rows", type=int, default=2000,
                     help="rows already in the child table before ADD CONSTRAINT "
                          "(default 2000)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --child-rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["enforced", "notvalid"], default="enforced",
                     help="'notvalid' adds the FK NOT VALID, skipping the "
                          "existing-row scan entirely -- a control to isolate "
                          "the scan's share of Ir by subtraction (default "
                          "'enforced')")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.child_rows]
    for n in sizes:
        r = run_one(args.tool, args.parent_rows, n,
                    keep_log=args.keep_log if not args.sizes else None,
                    variant=args.variant)
        if args.tool == "callgrind":
            print(f"parent_rows={args.parent_rows} child_rows={n} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"parent_rows={args.parent_rows} child_rows={n}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
