#!/usr/bin/env python3
r"""v0.94 benchmark: hash equi-join vs nested loop, int / numeric / float keys.

The v0.94 PG19-parity work adds per-key canonicalization
(Numeric::hash_key with BigUint magnitude + trailing-zero stripping,
canon_float_key). This measures its real cost on the release binary:

  --kind int      10k x 10k one-to-one integer equi-join (v0.93 baseline shape)
  --kind numeric  10k x 10k numeric equi-join (canonicalization-heavy)
  --kind float    10k x 10k float8 equi-join

Each kind runs twice: hash join enabled (default) and
RUSTGRES_NO_HASH_JOIN=1 (pure nested loop). Reports wall-clock times,
correctness checks, and the nested/hash speedup.

Usage: python3 tests/bench_v094_hashparity.py [--n 10000] [--kind int|numeric|float]
"""
import importlib.util
import os
import socket
import struct
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location(
    "hp", os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "protocol_test_v094_hashparity.py"))
hp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hp)

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "release", "rustgres")
PORT = 5444


def lit(kind, i):
    if kind == "int":
        return str(i)
    if kind == "numeric":
        # alternate scales to force cross-scale canonicalization: 5, 5.0, 5.00
        return f"{i}.{'0' * (i % 3)}" if i % 3 else str(i)
    # float: distinct values; -0.0/0.0 semantics are covered by the
    # protocol test, here we just measure the key-extraction path
    return f"{i}.5"


def coltype(kind):
    return {"int": "int", "numeric": "numeric", "float": "float8"}[kind]


def run_case(tag, n, kind, env_extra):
    datadir = f"/tmp/rg_bench_v094_{tag}_{kind}"
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
                c = hp.Conn(PORT)
                break
            except (ConnectionRefusedError, RuntimeError):
                time.sleep(0.2)
        assert c is not None, "server never came up"
        ct = coltype(kind)
        c.q(f"create table ba(id {ct});")
        c.q(f"create table bb(id {ct});")
        batch = 1000
        for lo in range(1, n + 1, batch):
            hi = min(lo + batch - 1, n)
            vals = ",".join(f"({lit(kind, i)})" for i in range(lo, hi + 1))
            r1, e1 = c.q(f"insert into ba values {vals};")
            r2, e2 = c.q(f"insert into bb values {vals};")
            assert e1 is None and e2 is None, f"insert failed: {e1} {e2}"
        sql = "select count(*), sum(ba.id) from ba join bb on ba.id = bb.id;"
        t0 = time.perf_counter()
        rows, err = c.q(sql)
        dt = time.perf_counter() - t0
        assert err is None, f"bench query failed: {err}"
        count, total = rows[0]
        # float keys are i.5, so the expected sum has an extra 0.5*n
        expect = n * (n + 1) / 2 + (0.5 * n if kind == "float" else 0)
        ok = (count == str(n) and abs(float(total) - expect) < 1.0)
        return dt, count, total, ok
    finally:
        proc.terminate()
        proc.wait()


def main():
    n = 10000
    kind = "int"
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        a = args[i]
        if a.startswith("--n="):
            n = int(a.split("=", 1)[1])
        elif a == "--n":
            i += 1
            n = int(args[i])
        elif a.startswith("--kind="):
            kind = a.split("=", 1)[1]
        elif a == "--kind":
            i += 1
            kind = args[i]
        i += 1
    assert kind in ("int", "numeric", "float"), kind
    if not os.path.exists(BIN):
        print(f"missing {BIN}; run `cargo build --release -j1` first")
        sys.exit(2)
    print(f"v0.94 hash-join benchmark [{kind}]: {n}x{n} inner equi-join (release)")
    results = {}
    for tag, env in (("hash", {}), ("nested", {"RUSTGRES_NO_HASH_JOIN": "1"})):
        dt, count, total, ok = run_case(tag, n, kind, env)
        results[tag] = dt
        status = "OK" if ok else "MISMATCH"
        print(f"  {tag:6s}: {dt:8.2f}s  count={count} sum={total} [{status}]")
    h, ne = results["hash"], results["nested"]
    if h > 0:
        print(f"  speedup: {ne / h:.1f}x (nested/hash)")


if __name__ == "__main__":
    main()
