#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for RLS target-visibility checks
against an ALIASED UPDATE target (v2.37 `src/exec/rls.rs`
`policy_scope_augment`).

Realistic shape: a multi-tenant SaaS table (every row carries a
`tenant_id`, RLS forced with one permissive tenant-isolation policy --
the same shape `profile_rls.py` uses for INSERT) updated the way
application code actually writes it: with a short alias on the target
(`UPDATE accounts AS a SET ... WHERE a.id = $1`), which is the
idiomatic form once a statement has a WHERE clause referencing the
table, let alone a JOIN/FROM. `profile_rls.py`'s 2026-10-20 negative
result profiled the INSERT path, which never aliases its own target
and so never exercises this.

v2.37 added `policy_scope_augment` (src/exec/rls.rs): PG19 binds a
policy's qual to the table's bare name regardless of the query's
alias, so when the row's schema is qualified by anything other than
the bare table name, every per-row policy check call clones the
*whole* column schema (`Vec<QCol>`, each entry two owned `String`s)
AND the row's cells, doubling both, to add a hidden bare-name-qualified
copy -- even though that schema augmentation is identical on every row
of one statement. Only the per-row cell values differ.

`exec_update_inner`'s target-collection loop (`src/exec/dml.rs`) also
collects every MVCC-visible row of the table into `vis` before
filtering by WHERE, so each UPDATE's RLS visibility pass costs is
proportional to total table row count, not to how selective the WHERE
clause is -- `--rows` controls that lever directly.

Usage:
    cargo build
    python3 benches/profile_rls_alias_update.py --tool callgrind --rows 2000 --updates 20
    python3 benches/profile_rls_alias_update.py --tool dhat --rows 2000 --updates 20

    # a few row counts in one go, printing just the Ir totals:
    python3 benches/profile_rls_alias_update.py --tool callgrind --sizes

    # sanity check: same workload, no alias on the UPDATE target (the
    # policy_scope_augment fast path -- schema qual already matches the
    # bare table name, so no clone):
    python3 benches/profile_rls_alias_update.py --tool callgrind --rows 2000 --updates 20 --no-alias
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


def run_one(tool, rows, updates, alias, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgrlsalias_")
    log_dir = tempfile.mkdtemp(prefix="rgrlsalias_log_")
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
        w.simple("DROP TABLE IF EXISTS bench_rls_alias")
        w.simple(
            "CREATE TABLE bench_rls_alias(id INT, tenant_id INT, "
            "c1 TEXT, c2 TEXT, c3 TEXT, c4 TEXT, val INT)"
        )
        w.simple("ALTER TABLE bench_rls_alias ENABLE ROW LEVEL SECURITY")
        w.simple("ALTER TABLE bench_rls_alias FORCE ROW LEVEL SECURITY")
        w.simple(
            "CREATE POLICY tenant_isolation ON bench_rls_alias "
            "USING (tenant_id = 1) WITH CHECK (tenant_id = 1)"
        )
        # Bulk-load `rows` tenant-1 rows in VALUES batches (application
        # schema shape: a handful of text columns alongside the tenant
        # key, same column count profile_catalog_scan.py uses per-table).
        batch = 500
        i = 0
        while i < rows:
            n = min(batch, rows - i)
            vals = ",".join(
                f"({j},1,'c1-{j}','c2-{j}','c3-{j}','c4-{j}',0)"
                for j in range(i, i + n)
            )
            w.simple(f"INSERT INTO bench_rls_alias VALUES {vals}")
            i += n

        # Fixed-iteration single-row UPDATEs against the RLS-protected
        # table, through an alias -- the idiomatic form of any UPDATE
        # with a WHERE clause (and the only form once a FROM/JOIN is
        # involved). `--no-alias` runs the same statements unaliased,
        # the policy_scope_augment fast path, for comparison.
        tgt = "bench_rls_alias AS b" if alias else "bench_rls_alias"
        col = "b.id" if alias else "id"
        for i in range(updates):
            w.simple(f"UPDATE {tgt} SET val = val + 1 WHERE {col} = {i % rows}")
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
                     help="rows in bench_rls_alias, all tenant_id=1 (default 2000)")
    ap.add_argument("--updates", type=int, default=20,
                     help="single-row UPDATEs against the table (default 20)")
    ap.add_argument("--no-alias", dest="alias", action="store_false",
                     help="run the UPDATEs without a target alias (fast-path comparison)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.rows]
    for n in sizes:
        r = run_one(args.tool, n, args.updates, args.alias,
                    keep_log=args.keep_log if not args.sizes else None)
        if args.tool == "callgrind":
            print(f"rows={n} updates={args.updates} alias={args.alias} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"rows={n} updates={args.updates} alias={args.alias}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
