#!/usr/bin/env python3
"""Populates a data directory with a realistic crash-recovery WAL, for
profiling `wal::writer::Wal::open` / `wal::recovery::apply_record` -- the
one path that runs on *every* server restart, planned or a crash, before
the server accepts a single connection. No prior Bolt round has targeted
it (see benches/BASELINE.md); the existing SIGKILL soak tests exercise it
for correctness but nothing measures its cost.

Real deployments restart with a WAL built from many independent, small
commits (one row at a time, or a handful of rows per statement) -- not
one giant bulk load -- so this driver issues `--txns` separate autocommit
INSERTs (one row each), the shape the module docs call out explicitly:
"Autocommit statements are their own implicit transactions ... logged
... while the database lock is held". No CHECKPOINT is taken, so every
commit's frame stays in wal.log for recovery to replay from scratch, same
as a server that has been up for a while and restarts (or crashes)
between checkpoints.

Usage:
    cargo build
    python3 benches/profile_recovery.py --data-dir /tmp/rgrecov --txns 3000
    # data dir now holds a WAL with --txns committed single-row INSERTs
    # (plus the CREATE TABLE) and no checkpoint. Point a fresh,
    # valgrind-wrapped server at it to profile recovery:
    RUSTGRES_DATA_DIR=/tmp/rgrecov valgrind --tool=callgrind \\
      --callgrind-out-file=/tmp/cg.out ./target/debug/rustgres &
    # wait for the "recovery: ... record(s)" line on stdout, then
    # SIGTERM the pid to flush callgrind's output.
"""
import argparse
import os
import shutil
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"


def wait_for_port(host, port, proc, timeout=30.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"server exited early (code {proc.returncode})")
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError(f"server did not open {host}:{port} within {timeout}s")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=5434,
                     help="build-phase port (default 5434, distinct from "
                          "the usual 5433 so this can run alongside a "
                          "server already under valgrind)")
    ap.add_argument("--data-dir", required=True,
                     help="directory to populate; removed and recreated")
    ap.add_argument("--txns", type=int, default=3000,
                     help="number of separate autocommit single-row "
                          "INSERTs (default 3000)")
    ap.add_argument("--bin", default="./target/debug/rustgres",
                     help="server binary to run for the build phase")
    ap.add_argument("--timeout", type=float, default=60.0,
                     help="socket timeout in seconds")
    args = ap.parse_args()

    data_dir = os.path.abspath(args.data_dir)
    shutil.rmtree(data_dir, ignore_errors=True)
    os.makedirs(data_dir, exist_ok=True)

    env = dict(os.environ)
    env["RUSTGRES_DATA_DIR"] = data_dir
    env["RUSTGRES_PORT"] = str(args.port)
    proc = subprocess.Popen(
        [args.bin], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    try:
        wait_for_port(args.host, args.port, proc, timeout=args.timeout)

        conn = Conn(args.host, args.port)
        conn.s.settimeout(args.timeout)
        conn.simple("DROP TABLE IF EXISTS bench_recov")
        r = conn.simple(
            "CREATE TABLE bench_recov(id INT, name TEXT, amt NUMERIC)"
        )
        assert tag_of(r) == "CREATE TABLE", tag_of(r)

        for i in range(args.txns):
            sql = f"INSERT INTO bench_recov VALUES ({i}, 'name{i}', {i}.50)"
            r = conn.simple(sql)
            assert tag_of(r) == "INSERT 0 1", tag_of(r)
        conn.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    print(
        f"populated {data_dir} with 1 CREATE TABLE + {args.txns} "
        f"single-row autocommit INSERTs (no checkpoint); point a fresh "
        f"server's RUSTGRES_DATA_DIR at it to profile recovery",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
