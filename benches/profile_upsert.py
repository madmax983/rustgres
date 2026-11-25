#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for bulk `INSERT ... ON CONFLICT
... DO UPDATE` (upsert), src/exec/dml.rs exec_insert_inner's per-row
conflict-arbitration loop -- never profiled since ON CONFLICT landed
(v0.10).

Realistic shape: a batch-upsert job against a `users` table that has a
primary key plus a couple of secondary UNIQUE indexes (email, username)
-- the common "sync a batch of records, update if present" pattern
(ETL load, cache refresh, idempotent event processing). Half the rows
in each batch already exist (by id) and get DO UPDATE; half are new
and get inserted. The database also holds `--other-tables` unrelated
tables (each with `--indexes-per-other-table` of its own indexes), to
reflect a realistic multi-table schema -- `exec_insert_inner`'s
conflict check resolves each arbiter by calling
`Database::unique_conflict_row`, which (per src/storage.rs) calls
`visible_indexes_for`: a scan of *every* index in the whole database
(`self.indexes.values()`), filtered down to the target table and
sorted, just to then `.find()` the ONE already-known-by-name arbiter
index. That scan-filter-sort runs once per arbiter per row (plus again
per non-arbiter unique index when an explicit arbiter is given), so
its cost scales with the total number of indexes in the database, not
with the target table's own index count.

This harness isolates that cost two ways:
  * `--other-tables N`: scales the size of the irrelevant part of
    `self.indexes` that every arbiter lookup still has to scan past.
    The upsert's own shape (3 rows, 3 indexes, half-new/half-existing)
    stays fixed, so Ir growth with N isolates the catalog-wide-scan
    cost from the upsert's own inherent work.
  * `--op upsert` vs `--op plain_insert`: `INSERT ... ON CONFLICT (id)
    DO UPDATE ...` vs a same-shape `INSERT` with no ON CONFLICT clause
    at all (new rows only, same row count) -- diffing the two isolates
    the arbitration-loop cost from generic per-row INSERT/WAL/pgstat
    overhead.

Usage:
    cargo build
    python3 benches/profile_upsert.py --tool callgrind --rows 2000 --other-tables 30
    python3 benches/profile_upsert.py --tool callgrind --rows 2000 --other-tables 30 --op plain_insert
    python3 benches/profile_upsert.py --tool dhat --rows 2000 --other-tables 30

    # scan the other-tables axis in one go, printing Ir:
    python3 benches/profile_upsert.py --tool callgrind --other-tables-sizes
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
DEFAULT_OTHER_TABLE_SIZES = [0, 15, 30, 60]


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


def run_one(tool, rows, other_tables, indexes_per_other_table, op, chunk=200, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgup_")
    log_dir = tempfile.mkdtemp(prefix="rgup_log_")
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

        # Realistic multi-table schema: `--other-tables` unrelated tables,
        # each with its own PK plus `--indexes-per-other-table - 1`
        # secondary indexes, none of which are ever touched by the
        # measured statement. These only inflate the *total* index count
        # in the database (self.indexes in src/storage.rs), isolating
        # whether conflict arbitration's cost scales with it.
        for t in range(other_tables):
            w.simple(f"CREATE TABLE other_{t} (id INT PRIMARY KEY, a INT, b INT, c INT)")
            for k in range(indexes_per_other_table - 1):
                col = ["a", "b", "c"][k % 3]
                w.simple(f"CREATE INDEX other_{t}_{col}_{k}_idx ON other_{t}({col})")

        # The upsert target: a `users` table shaped like a real batch-sync
        # table -- PK plus two secondary UNIQUE indexes (the common
        # "upsert by id, keep email/username unique" shape).
        w.simple(
            "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, "
            "username TEXT UNIQUE, name TEXT, score INT)"
        )

        # Pre-populate half of `rows` as "existing" rows (ids 0..rows/2),
        # so the measured batch's matching ids hit the DO UPDATE path,
        # not just the insert path -- a realistic sync batch is a mix.
        half = rows // 2
        for start in range(0, half, chunk):
            end = min(start + chunk, half)
            vals = ",".join(
                f"({i},'old{i}@x.test','olduser{i}','Old Name {i}',{i})"
                for i in range(start, end)
            )
            w.simple(f"INSERT INTO users VALUES {vals}")

        # Build the measured batch: ids 0..half already exist (DO UPDATE),
        # ids half..rows are new (INSERT). Each row's email/username is
        # distinct so no secondary-unique conflict is ever hit -- this
        # isolates the PK-arbiter conflict-check cost, not constraint
        # violation handling.
        batch_vals = ",".join(
            f"({i},'new{i}@x.test','newuser{i}','New Name {i}',{i * 2})"
            for i in range(rows)
        )
        if op == "upsert":
            stmt = (
                f"INSERT INTO users VALUES {batch_vals} "
                "ON CONFLICT (id) DO UPDATE SET "
                "name = EXCLUDED.name, score = EXCLUDED.score"
            )
        else:
            # plain_insert baseline: same row count, same table/indexes,
            # but every id is new (no ON CONFLICT clause at all) -- isolates
            # generic per-row INSERT/WAL/pgstat cost from the arbitration
            # loop that only `upsert` exercises.
            plain_vals = ",".join(
                f"({i + rows},'p{i}@x.test','puser{i}','P Name {i}',{i})"
                for i in range(rows)
            )
            stmt = f"INSERT INTO users VALUES {plain_vals}"

        # Measured statement: one batch upsert/insert touching `rows` rows.
        w.simple(stmt)
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
                     help="rows in the measured batch (half DO UPDATE, half INSERT); default 2000")
    ap.add_argument("--other-tables", type=int, default=30,
                     help="unrelated tables elsewhere in the database (default 30)")
    ap.add_argument("--indexes-per-other-table", type=int, default=3,
                     help="indexes per other table, including its PK (default 3)")
    ap.add_argument("--op", choices=["upsert", "plain_insert"], default="upsert",
                     help="upsert = INSERT ... ON CONFLICT (id) DO UPDATE; "
                          "plain_insert = baseline, same row count, no ON CONFLICT")
    ap.add_argument("--other-tables-sizes", action="store_true",
                     help=f"run --other-tables at {DEFAULT_OTHER_TABLE_SIZES} in one go "
                          "(fixed --rows), printing Ir")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    if args.other_tables_sizes:
        for n in DEFAULT_OTHER_TABLE_SIZES:
            r = run_one(args.tool, args.rows, n, args.indexes_per_other_table, args.op)
            if args.tool == "callgrind":
                print(f"rows={args.rows} other_tables={n} op={args.op} Ir={r.get('Ir')}")
        return

    r = run_one(args.tool, args.rows, args.other_tables, args.indexes_per_other_table,
                args.op, keep_log=args.keep_log)
    if args.tool == "callgrind":
        print(f"rows={args.rows} other_tables={args.other_tables} op={args.op} Ir={r.get('Ir')}")
    else:
        lines = [ln for ln in r["raw"].splitlines()
                 if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
        print(f"rows={args.rows} other_tables={args.other_tables} op={args.op}")
        for ln in lines[:6]:
            print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
