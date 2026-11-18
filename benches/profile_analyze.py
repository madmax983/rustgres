#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for ANALYZE (src/exec/analyze.rs,
src/exec/statistic.rs, src/exec/extstats.rs), v2.33's new pg_statistic
census code -- never profiled before this harness.

Realistic shape: a DBA/autovacuum running `ANALYZE` against a table with
a realistic column mix -- a low-cardinality category column and a bool
flag (exercise the MCV path), a mostly-unique text column (histogram +
avg_width), a numeric amount column (distinct_raw / correlation), and
an int[] array column (MCELEM/DECHIST, v2.33's element-statistics code,
exercised nowhere else in benches/). An expression index
`((cat || name))` on the table also drives `analyze_index_exprs`, the
v2.33 code that evaluates index expressions over every visible row to
compute their own pg_statistic entry.

`--rounds` repeats `ANALYZE bench_analyze` against the same (static)
data: ANALYZE does not mutate rows, so repeating is exact replicated
work, the same trick `profile_catalog_scan.py` uses for its `--queries`
loop, giving a clean multiplier on top of the one-shot per-table cost.

Usage:
    cargo build
    python3 benches/profile_analyze.py --tool callgrind --rows 4000 --rounds 20
    python3 benches/profile_analyze.py --tool dhat --rows 4000 --rounds 20

    # several row counts in one go, printing just the Ir totals:
    python3 benches/profile_analyze.py --tool callgrind --sizes
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
DEFAULT_SIZES = [1000, 2000, 4000]


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


CATEGORIES = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]


def row_values(i):
    cat = CATEGORIES[i % len(CATEGORIES)]
    name = f"item-{i}-{'x' * (i % 17)}"
    amount = f"{(i % 997) + 0.5:.2f}"
    tags = "ARRAY[{},{},{}]".format(i % 5, (i * 3) % 11, (i * 7) % 13)
    flag = "true" if i % 3 == 0 else "false"
    return f"({i},'{cat}','{name}',{amount},{tags},{flag})"


def run_one(tool, rows, rounds, chunk=500, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rganalyze_")
    log_dir = tempfile.mkdtemp(prefix="rganalyze_log_")
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
        w.simple(
            "CREATE TABLE bench_analyze(id int, cat text, name text, "
            "amount numeric, tags int[], flag bool)"
        )
        # v2.33: index-expression statistics (analyze_index_exprs).
        w.simple("CREATE INDEX bench_analyze_expr_idx ON bench_analyze((cat || name))")
        for start in range(0, rows, chunk):
            end = min(start + chunk, rows)
            vals = ",".join(row_values(i) for i in range(start, end))
            w.simple(f"INSERT INTO bench_analyze VALUES {vals}")

        # Fixed-iteration ANALYZE rounds -- the periodic/manual-DBA
        # restat pattern. The data never changes, so each round is
        # exact replicated work.
        for _ in range(rounds):
            w.simple("ANALYZE bench_analyze")
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
    ap.add_argument("--rows", type=int, default=4000,
                     help="rows in bench_analyze (default 4000)")
    ap.add_argument("--rounds", type=int, default=20,
                     help="repeated ANALYZE bench_analyze statements (default 20)")
    ap.add_argument("--sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_SIZES} in one go")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    sizes = DEFAULT_SIZES if args.sizes else [args.rows]
    for n in sizes:
        r = run_one(args.tool, n, args.rounds,
                     keep_log=args.keep_log if not args.sizes else None)
        if args.tool == "callgrind":
            print(f"rows={n} rounds={args.rounds} Ir={r.get('Ir')}")
        else:
            lines = [ln for ln in r["raw"].splitlines()
                     if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
            print(f"rows={n} rounds={args.rounds}")
            for ln in lines[:6]:
                print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
