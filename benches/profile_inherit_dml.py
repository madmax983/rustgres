#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for per-statement full-schema
scans on the UPDATE path: `src/exec/dml.rs` `inheritance_targets`
(v2.43, called whenever the statement has no `ONLY`) and
`src/exec/constraint.rs` `fks_referencing` (called from
`plan_fk_cascade` whenever a row actually changes, to find every OTHER
table whose FK references the one being updated). Neither has a
reverse index ("children of this table" / "FKs referencing this
table"), so discovering "this table has none" costs O(tables in the
database), every single statement, whether or not the schema uses
inheritance or foreign keys at all.

Realistic shape: an application schema with many ordinary tables (no
inheritance, no foreign keys anywhere in it -- the overwhelmingly
common case for a real app's bulk of tables) -- then an OLTP-style
loop of single-row UPDATEs against ONE of those tables, as e.g. a
counter-bump or status-flag update workload would issue.

Three statement variants:

  --op update   UPDATE pt0 SET val=... WHERE id=1      (realistic: row
                matches, no ONLY)
  --op only     UPDATE ONLY pt0 SET val=... WHERE id=1 (row matches,
                ONLY -- skips inheritance_targets only).
                `update - only` isolates inheritance_targets' cost.
  --op nomatch  UPDATE pt0 SET val=... WHERE id=999999 (no ONLY, but
                the WHERE matches no row, so no mutation happens at
                all: no WAL append, no trigger firing, no pgstat
                accounting, no index maintenance, AND no
                plan_fk_cascade/fks_referencing call).
                `update - nomatch` is the combined cost of actually
                applying one row change -- useful context, but it is
                NOT fks_referencing's cost alone; isolating that one
                function required a one-off source-level ablation
                (stubbing it to `return Vec::new()`), not a runtime
                flag -- see the PR/issue writeup for those numbers.
                An earlier draft of this harness treated `update -
                nomatch` as fks_referencing's isolated cost; the
                ablation showed that was wrong by more than an order
                of magnitude (it actually measures the whole
                row-mutation path, of which fks_referencing is a
                small part) -- kept here as a cautionary comparison,
                not a measurement of that one function.

Usage:
    cargo build
    python3 benches/profile_inherit_dml.py --tool callgrind --tables 150 --updates 50 --op update
    python3 benches/profile_inherit_dml.py --tool callgrind --tables 150 --updates 50 --op only
    python3 benches/profile_inherit_dml.py --tool callgrind --tables 150 --updates 50 --op nomatch
    python3 benches/profile_inherit_dml.py --tool dhat --tables 150 --updates 50 --op update

    # several schema sizes in one go, printing just the Ir totals:
    python3 benches/profile_inherit_dml.py --tool callgrind --op update --sizes
    python3 benches/profile_inherit_dml.py --tool callgrind --op nomatch --sizes
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
DEFAULT_SIZES = [10, 40, 150]


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


def run_one(tool, tables, updates, op, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rginherit_")
    log_dir = tempfile.mkdtemp(prefix="rginherit_log_")
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
        # `tables` ordinary tables, none using inheritance and none
        # carrying any foreign key -- the common real-world schema
        # shape. pt0 is the UPDATE target; the rest exist only so
        # inheritance_targets()/fks_referencing() have somewhere to
        # waste a scan.
        for t in range(tables):
            w.simple(f"CREATE TABLE pt{t}(id int PRIMARY KEY, val int)")
            w.simple(f"INSERT INTO pt{t} VALUES (1, 0)")

        # Fixed-iteration single-row UPDATEs against ONE table (pt0),
        # the common counter-bump / status-flag OLTP pattern.
        #   update  -> plain form every app actually writes: pays both
        #              inheritance_targets and (since the row matches
        #              and changes) fks_referencing via plan_fk_cascade.
        #   only    -> adds ONLY: skips inheritance_targets, still pays
        #              fks_referencing. Isolates inheritance_targets'
        #              cost as (update - only).
        #   nomatch -> plain form but WHERE matches no row: still pays
        #              inheritance_targets, but skips the entire
        #              row-mutation path (WAL, triggers, pgstat,
        #              indexes, AND plan_fk_cascade/fks_referencing).
        #              (update - nomatch) is that whole path's cost,
        #              NOT fks_referencing's alone -- see module
        #              docstring.
        if op == "only":
            stmt = "UPDATE ONLY pt0 SET val = {i} WHERE id = 1"
        elif op == "nomatch":
            stmt = "UPDATE pt0 SET val = {i} WHERE id = 999999"
        else:
            stmt = "UPDATE pt0 SET val = {i} WHERE id = 1"
        for i in range(updates):
            w.simple(stmt.format(i=i))
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
    ap.add_argument("--tables", type=int, default=40,
                     help="number of ordinary (non-inheriting, FK-free) tables in the schema (default 40)")
    ap.add_argument("--updates", type=int, default=50,
                     help="single-row UPDATE statements against table pt0 (default 50)")
    ap.add_argument("--op", choices=["update", "only", "nomatch"], default="update",
                     help="'update' = plain UPDATE pt0, row matches (pays both scans); "
                          "'only' = UPDATE ONLY pt0, row matches (skips inheritance_targets); "
                          "'nomatch' = plain UPDATE pt0, WHERE matches no row "
                          "(skips the whole row-mutation path, not just fks_referencing)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --tables at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.tables]
    for n in sizes:
        r = run_one(args.tool, n, args.updates, args.op,
                    keep_log=args.keep_log if not args.sizes else None)
        if args.tool == "callgrind":
            print(f"tables={n} updates={args.updates} op={args.op} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"tables={n} updates={args.updates} op={args.op}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
