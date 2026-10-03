#!/usr/bin/env python3
r"""v0.93 benchmark: hash equi-join vs nested loop (RUSTGRES_NO_HASH_JOIN=1).

Runs a 10k x 10k one-to-one integer equi-join in release mode, both with the
hash join enabled (default) and disabled, and reports wall-clock times plus
correctness checks (row count and checksum). Times are honest wall-clock
measurements of the full query round-trip; the nested-loop run is slow by
design (that is the point of the comparison).

Usage: python3 tests/bench_v093_hashjoin.py [--n 10000]
"""
import importlib.util
import os
import socket
import struct
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location(
    "hj", os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "protocol_test_v093_hashjoin.py"))
hj = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hj)

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "release", "rustgres")
PORT = 5443


def run_case(tag, n, env_extra):
    datadir = f"/tmp/rg_bench_v093_{tag}"
    os.system(f"rm -rf {datadir}")
    e = dict(os.environ)
    e.update(env_extra)
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", datadir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=e)
    try:
        c = None
        for _ in range(100):
            try:
                c = hj.Conn(PORT)
                break
            except (ConnectionRefusedError, RuntimeError):
                time.sleep(0.2)
        assert c is not None, "server never came up"
        c.q("create table ba(id int);")
        c.q("create table bb(id int);")
        # insert in batches to keep each statement modest
        batch = 1000
        for lo in range(1, n + 1, batch):
            hi = min(lo + batch - 1, n)
            vals = ",".join(f"({i})" for i in range(lo, hi + 1))
            c.q(f"insert into ba values {vals};")
            c.q(f"insert into bb values {vals};")
        sql = "select count(*), sum(ba.id) from ba join bb on ba.id = bb.id;"
        t0 = time.perf_counter()
        rows, err = c.q(sql)
        dt = time.perf_counter() - t0
        assert err is None, f"bench query failed: {err}"
        count, total = rows[0]
        expect_total = str(n * (n + 1) // 2)
        ok = (count == str(n) and total == expect_total)
        return dt, count, total, ok
    finally:
        proc.terminate()
        proc.wait()


def main():
    n = 10000
    args = sys.argv[1:]
    for i, a in enumerate(args):
        if a.startswith("--n="):
            n = int(a.split("=", 1)[1])
        elif a == "--n" and i + 1 < len(args):
            n = int(args[i + 1])
    if not os.path.exists(BIN):
        print(f"missing {BIN}; run `cargo build --release -j1` first")
        sys.exit(2)
    print(f"v0.93 hash-join benchmark: {n}x{n} inner equi-join (release)")
    results = {}
    for tag, env in (("hash", {}), ("nested", {"RUSTGRES_NO_HASH_JOIN": "1"})):
        dt, count, total, ok = run_case(tag, n, env)
        results[tag] = dt
        status = "OK" if ok else "MISMATCH"
        print(f"  {tag:6s}: {dt:8.2f}s  count={count} sum={total} [{status}]")
    h, ne = results["hash"], results["nested"]
    if h > 0:
        print(f"  speedup: {ne / h:.1f}x (nested/hash)")
    # honesty: nested-loop cost is quadratic; report per-pair rate too
    pairs = n * n
    print(f"  nested pair rate: {pairs / ne / 1e6:.2f}M pairs/s")


if __name__ == "__main__":
    main()
