#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for bulk INSERT into a table with
a `DO ALSO` rule -- the classic audit-log rule shape from Jan's original
ruletest suite (`rtest_emp_ins`/`rtest_emp_upd` in
tests/conformance/pg19/sql/rules.sql): every inserted row fires a second
INSERT into a log table via `exec_insert_with_rules` /
`fire_rule_actions` (src/exec/rules.rs).

CREATE RULE (v2.19) has never been profiled by a prior Bolt harness.
`fire_rule_actions` deep-clones the rule's action statement AST
(`action.stmt.clone()`) and re-substitutes NEW/OLD into it on every
firing row, which is the same per-row-rebuild shape that was already
found and fixed for view-DML `eval_scoped` (PR #69) and FK/trigger row
checks (PRs #64, #67) in this codebase.

Launches its own `rustgres` server instance directly under valgrind
(mirrors `profile_trigger_insert.py`'s self-contained pattern).

`--rows` sizes the single multi-row INSERT. `--variant rule` creates the
one-action DO ALSO logging rule that every row's INSERT must fire;
`--variant norule` is a control with no rule at all, isolating the
rule-firing subtree's share of Ir by subtraction.

Usage:
    cargo build
    python3 benches/profile_rule_insert.py --tool callgrind --rows 2000
    python3 benches/profile_rule_insert.py --tool dhat --rows 2000

    # all three sizes in one go, printing just the Ir/bytes totals:
    python3 benches/profile_rule_insert.py --tool callgrind --sizes
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


def run_one(tool, rows, keep_log=None, variant="rule"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgruleprof_")
    log_dir = tempfile.mkdtemp(prefix="rgruleprof_log_")
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
        w.simple(
            "CREATE TABLE bench_rule_emp(ename TEXT, salary NUMERIC)"
        )
        w.simple(
            "CREATE TABLE bench_rule_log(ename TEXT, who TEXT, "
            "action TEXT, newsal NUMERIC)"
        )
        if variant == "rule":
            # Mirrors rtest_emp_ins: every INSERT row also logs a row
            # via NEW substitution -- no WHEN qual, DO ALSO (not INSTEAD),
            # so the original insert runs too, same as the real test.
            w.simple(
                "CREATE RULE bench_rule_ins AS ON INSERT TO bench_rule_emp "
                "DO ALSO INSERT INTO bench_rule_log VALUES "
                "(new.ename, 'bench', 'hired', new.salary)"
            )
        # The one statement under measurement: a single multi-row INSERT
        # of `rows` rows, each row's rule firing (variant 'rule') or not
        # (variant 'norule'). 'doubleinsert' is a control with no rule
        # that does the same total row volume (rows into emp, rows into
        # log) as two ordinary *batched* multi-row INSERTs -- isolating
        # how much of the 'rule' variant's cost is the inherent work of
        # writing 2*rows rows across two tables vs. the per-row-firing
        # overhead of doing it as `rows` separate single-row statements.
        w.simple(
            f"INSERT INTO bench_rule_emp SELECT 'emp' || g, g * 1.5 "
            f"FROM generate_series(1, {rows}) g"
        )
        if variant == "doubleinsert":
            w.simple(
                f"INSERT INTO bench_rule_log SELECT 'emp' || g, 'bench', "
                f"'hired', g * 1.5 FROM generate_series(1, {rows}) g"
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
    ap.add_argument("--rows", type=int, default=1000,
                     help="rows in the single multi-row INSERT (default 1000)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["rule", "norule", "doubleinsert"], default="rule",
                     help="'norule' omits the rule entirely -- a control "
                          "to isolate the rule-firing subtree's share of "
                          "Ir by subtraction (default 'rule')")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.rows]
    for n in sizes:
        r = run_one(args.tool, n,
                    keep_log=args.keep_log if not args.sizes else None,
                    variant=args.variant)
        if args.tool == "callgrind":
            print(f"rows={n} variant={args.variant} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"rows={n} variant={args.variant}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
