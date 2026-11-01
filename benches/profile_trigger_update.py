#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for a bulk UPDATE against a table
with a BEFORE UPDATE FOR EACH ROW trigger -- an ordinary audit/counter
trigger shape (bump a revision counter, stamp a column) fired once per
updated row via `fire_before_update_triggers` (src/exec/partition.rs).

Unlike the BEFORE INSERT path (`fire_before_insert_triggers`, see
`profile_trigger_insert.py`), which resolves the trigger function and
parses its plpgsql body *once* before looping over rows,
`fire_before_update_triggers` is called once per planned row (from the
UPDATE executor's per-row loop in src/exec/dml.rs) and re-resolves
`find_function_by_signature` and re-parses `parse_trigger_body` from
the function's source text on *every* row. This harness isolates that
per-row re-parse cost.

Launches its own `rustgres` server instance directly under valgrind
(mirrors `profile_fk_insert.py` / `profile_trigger_insert.py`'s
self-contained pattern), so one run of this script produces one
complete, parseable profile.

`--rows` sizes the table (and the single UPDATE touches all of it).
`--variant trig` creates a plpgsql BEFORE UPDATE FOR EACH ROW trigger;
`--variant notrig` is a control with no trigger, isolating the
trigger-firing subtree's share of Ir by subtraction.

Usage:
    cargo build
    python3 benches/profile_trigger_update.py --tool callgrind --rows 2000
    python3 benches/profile_trigger_update.py --tool dhat --rows 2000

    # all three sizes in one go, printing just the Ir totals:
    python3 benches/profile_trigger_update.py --tool callgrind --sizes
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


def run_one(tool, rows, keep_log=None, variant="trig"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgtrigupdprof_")
    log_dir = tempfile.mkdtemp(prefix="rgtrigupdprof_log_")
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
            "CREATE TABLE bench_trig_u(id INT PRIMARY KEY, val TEXT, cnt INT DEFAULT 0)"
        )
        w.simple(
            f"INSERT INTO bench_trig_u SELECT g, 'val' || g, 0 "
            f"FROM generate_series(1, {rows}) g"
        )
        if variant == "trig":
            w.simple(
                "CREATE FUNCTION bench_trig_upd_fn() RETURNS TRIGGER LANGUAGE plpgsql AS $$"
                " BEGIN"
                "   NEW.cnt := OLD.cnt + 1;"
                "   RETURN NEW;"
                " END; $$"
            )
            w.simple(
                "CREATE TRIGGER bench_trig_upd BEFORE UPDATE ON bench_trig_u "
                "FOR EACH ROW EXECUTE FUNCTION bench_trig_upd_fn()"
            )
        # The one statement under measurement: a single UPDATE touching
        # every row, each row's trigger firing (variant 'trig') or not
        # (variant 'notrig').
        w.simple("UPDATE bench_trig_u SET val = val || 'x'")
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
                     help="rows in the table / touched by the UPDATE (default 1000)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["trig", "notrig"], default="trig",
                     help="'notrig' omits the trigger entirely -- a control "
                          "to isolate the trigger-firing subtree's share of "
                          "Ir by subtraction (default 'trig')")
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
