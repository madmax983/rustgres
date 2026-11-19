#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for the v2.34 pgstat UPDATE hook
(src/exec/dml.rs exec_update, the HOT/not-HOT accounting for
pg_stat_*_tables.n_tup_hot_upd).

Realistic shape: an application schema with many tables, each carrying
a handful of indexes (the same density profile_catalog_scan.py uses for
schema-reflection tooling) -- then an OLTP-style loop of single-row
UPDATEs against ONE of those tables, as e.g. a counter-bump or
status-flag update workload would issue.

Every completed UPDATE calls `visible_index_defs()` (src/exec/catalog.rs)
to decide whether the SET columns are indexed (for HOT-update
accounting). That function clones and sorts EVERY visible index
definition in the whole database, then dml.rs's hook keeps only the
entries for the one table being updated. The cost of a single UPDATE
should depend on that table's own index count, not on how many indexes
exist elsewhere in the schema.

Usage:
    cargo build
    python3 benches/profile_pgstat_update.py --tool callgrind --tables 40 --indexes-per-table 4 --updates 50
    python3 benches/profile_pgstat_update.py --tool dhat --tables 40 --indexes-per-table 4 --updates 50

    # several schema sizes in one go, printing just the Ir totals:
    python3 benches/profile_pgstat_update.py --tool callgrind --sizes
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
DEFAULT_SIZES = [10, 25, 40]


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


def run_one(tool, tables, indexes_per_table, updates, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgpgstat_")
    log_dir = tempfile.mkdtemp(prefix="rgpgstat_log_")
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
        # `--indexes-per-table` secondary btree indexes per table, each
        # on its own (unindexed-by-the-UPDATE) column, plus the implicit
        # primary key index -- the schema-wide index count the pgstat
        # UPDATE hook has no business paying for on every statement.
        ncols = indexes_per_table + 2  # + val (updated, never indexed), + c0..c{n-1}
        for t in range(tables):
            cols = ", ".join(f"c{i} int" for i in range(indexes_per_table))
            w.simple(f"CREATE TABLE pt{t}(id int PRIMARY KEY, val int, {cols})")
            for i in range(indexes_per_table):
                w.simple(f"CREATE INDEX pt{t}_c{i}_idx ON pt{t}(c{i})")
            w.simple(f"INSERT INTO pt{t} VALUES (1, 0" + ", 0" * indexes_per_table + ")")

        # Fixed-iteration single-row UPDATEs against ONE table (pt0),
        # on its non-indexed `val` column -- the common counter-bump /
        # status-flag OLTP pattern. Every one of these completes
        # through exec_update's pgstat hook.
        for i in range(updates):
            w.simple(f"UPDATE pt0 SET val = {i} WHERE id = 1")
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
                     help="number of tables (default 40)")
    ap.add_argument("--indexes-per-table", type=int, default=4,
                     help="secondary indexes per table, plus the implicit PK index (default 4)")
    ap.add_argument("--updates", type=int, default=50,
                     help="single-row UPDATEs against table pt0 (default 50)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --tables at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.tables]
    for n in sizes:
        r = run_one(args.tool, n, args.indexes_per_table, args.updates,
                    keep_log=args.keep_log if not args.sizes else None)
        if args.tool == "callgrind":
            print(f"tables={n} indexes_per_table={args.indexes_per_table} "
                  f"updates={args.updates} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"tables={n} indexes_per_table={args.indexes_per_table} updates={args.updates}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
