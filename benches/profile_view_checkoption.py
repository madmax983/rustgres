#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for batched INSERTs through a
`WITH CHECK OPTION` view (src/exec/viewdml.rs). A view carrying CHECK
OPTION forces "capture mode": `exec_insert_auto_view` runs the base
INSERT, then re-derives the view-space row for every inserted base
row and evaluates the view's qual against it via `enforce_checks` ->
`eval_scoped`. No prior Bolt harness has profiled this path -- the
v2.18 auto-updatable-views feature (just landed) has not been
profiled at all yet.

`--variant checked` (default) creates the view WITH CHECK OPTION, so
every inserted row goes through capture mode (enforce_checks +
view_row_from_base). `--variant plain` creates the identical view
without CHECK OPTION, so `exec_insert_auto_view` takes the
no-capture fast path and forwards straight to the base-table INSERT --
a control that isolates CHECK OPTION's capture-mode share of the
statement's cost by subtraction (same technique as
`profile_serial_insert.py`'s serial/explicit split).

Launches its own `rustgres` server instance directly under valgrind
(same self-contained pattern as `profile_fk_addconstraint.py`).

Usage:
    cargo build
    python3 benches/profile_view_checkoption.py --tool callgrind --rows 20000
    python3 benches/profile_view_checkoption.py --tool dhat --rows 20000

    # both variants at one size, printing just the Ir/bytes totals:
    python3 benches/profile_view_checkoption.py --tool callgrind --rows 20000 --both
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
    data_dir = tempfile.mkdtemp(prefix="rgviewprof_")
    log_dir = tempfile.mkdtemp(prefix="rgviewprof_log_")
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
            "CREATE TABLE bench_orders(id SERIAL PRIMARY KEY, tenant_id INT, "
            "amount NUMERIC, status TEXT)"
        )
        if variant == "checked":
            w.simple(
                "CREATE VIEW bench_tenant_orders AS SELECT * FROM bench_orders "
                "WHERE tenant_id = 1 WITH CHECK OPTION"
            )
        else:
            w.simple(
                "CREATE VIEW bench_tenant_orders AS SELECT * FROM bench_orders "
                "WHERE tenant_id = 1"
            )
        # Every row's tenant_id is 1, so every row passes the check (we are
        # isolating capture-mode overhead, not the rejection path).
        w.simple(
            "INSERT INTO bench_tenant_orders(tenant_id, amount, status) "
            f"SELECT 1, g, 'open' FROM generate_series(1, {rows}) g"
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
    ap.add_argument("--variant", choices=["checked", "plain"], default="checked",
                     help="'checked' (default) creates the view WITH CHECK "
                          "OPTION so every row goes through capture mode; "
                          "'plain' is the identical view without CHECK "
                          "OPTION, a control with zero capture-mode calls")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_ROWS} in one go")
    ap.add_argument("--both", action="store_true",
                     help="run both variants at --rows in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_ROWS if args.sizes else [args.rows]
    variants = ["checked", "plain"] if args.both else [args.variant]
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
