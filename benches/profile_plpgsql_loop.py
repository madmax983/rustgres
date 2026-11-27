#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for a PL/pgSQL `FOR ... LOOP`
that runs one DML statement per iteration -- the common batch-job /
data-migration shape (`FOR i IN 1..N LOOP INSERT ...; END LOOP;`),
driven through a single `CALL`/`SELECT fn()` the way an application
would invoke a stored procedure, not a synthetic microbenchmark of a
single helper function.

Each loop iteration that runs a SQL statement goes through
`exec_plpgsql_stmt` (src/exec/funcs.rs), which does:

    let mut bound = stmt.clone();        // clone the whole statement AST
    bind_plpgsql_stmt(&mut bound, ...)    // -> run.params.clone() + two
                                           //    full-tree substitution walks
                                           //    (rewrite_record_refs_stmt,
                                           //    subst_params)

every single iteration, even though the loop body's AST is fixed and
only the current value of the loop variable changes. This harness
isolates that per-iteration rebind cost from the inherent per-row
INSERT cost (WAL append, storage insert, index maintenance, pgstat) by
comparing two loop bodies over the same iteration count:

  --variant insert : `INSERT INTO t(a) VALUES (i);`      (through
                      exec_plpgsql_stmt -> stmt.clone() + bind)
  --variant assign : `tot := tot + i;`                   (plain
                      plpgsql assignment -- no Stmt clone/bind at all)

Both variants run the identical `FOR i IN 1..N LOOP ... END LOOP;`
scaffold, called once via `SELECT bench_loop_fn(N)`, so the delta
between `insert` and a `--variant insert --rows 0`-equivalent baseline
isolates loop-body cost from one-time parse/plan overhead, and
`assign` gives the floor for "loop iterates but does no per-iteration
statement rebind" to sanity check that iteration itself is cheap.

Launches its own `rustgres` server instance directly under valgrind
(mirrors `profile_trigger_update.py`'s self-contained pattern), so one
run of this script produces one complete, parseable profile.

Usage:
    cargo build
    python3 benches/profile_plpgsql_loop.py --tool callgrind --rows 2000 --variant insert
    python3 benches/profile_plpgsql_loop.py --tool callgrind --rows 2000 --variant assign
    python3 benches/profile_plpgsql_loop.py --tool dhat --rows 2000 --variant insert

    # all default sizes in one go, printing just the Ir totals:
    python3 benches/profile_plpgsql_loop.py --tool callgrind --sizes --variant insert
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


def run_one(tool, rows, keep_log=None, variant="insert"):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgplloopprof_")
    log_dir = tempfile.mkdtemp(prefix="rgplloopprof_log_")
    log = os.path.join(log_dir, f"{tool}.vglog")
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
        if variant == "insert":
            w.simple("CREATE TABLE bench_loop_t(a INT)")
            w.simple(
                "CREATE FUNCTION bench_loop_fn(n INT) RETURNS VOID LANGUAGE plpgsql AS $$"
                " BEGIN"
                "   FOR i IN 1..n LOOP"
                "     INSERT INTO bench_loop_t(a) VALUES (i);"
                "   END LOOP;"
                " END; $$"
            )
        else:  # assign
            w.simple(
                "CREATE FUNCTION bench_loop_fn(n INT) RETURNS BIGINT LANGUAGE plpgsql AS $$"
                " DECLARE tot BIGINT := 0;"
                " BEGIN"
                "   FOR i IN 1..n LOOP"
                "     tot := tot + i;"
                "   END LOOP;"
                "   RETURN tot;"
                " END; $$"
            )
        # The one statement under measurement: a single call running
        # the whole loop server-side, the way an application invokes a
        # batch-processing stored procedure.
        w.simple(f"SELECT bench_loop_fn({rows})")
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
                     help="loop iteration count N (default 1000)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    ap.add_argument("--variant", choices=["insert", "assign"], default="insert",
                     help="'insert' runs a DML statement per iteration "
                          "(through exec_plpgsql_stmt's clone+bind path); "
                          "'assign' is a control with no per-iteration "
                          "statement rebind (default 'insert')")
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
