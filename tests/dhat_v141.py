#!/usr/bin/env python3
"""dhat_v141.py — DHAT heap profile for the v1.41 feature paths.

Workload: pg_relation_size page-accounting scans over many rows (tuple
layout per visible row version), attnum churn (drop/add column
sequences), partition attnum inheritance, checkpoint with the extended
table image encoding. Uses the DEBUG binary.
"""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5591
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "create table dh141(a int, b text, c numeric) with (fillfactor = 20)",
    "insert into dh141 select g, repeat('v', 100), g::numeric * 1.5 from generate_series(1, 2000) g",
    # pg_relation_size scans all visible row versions with tuple layout
    "select pg_relation_size('dh141')",
    "select pg_size_pretty(pg_relation_size('dh141'::regclass, 'main'))",
    # attnum churn
    "create table dh141c (a int, b int, c int)",
    "alter table dh141c drop column a",
    "alter table dh141c add column a int",
    "alter table dh141c drop column b",
    "alter table dh141c add column b text",
    "select attname, attnum from pg_attribute where attrelid = 'dh141c'::regclass",
    # partition inheritance
    "create table dh141p (a int, b int) partition by range (a)",
    "create table dh141p1 partition of dh141p for values from (1) to (100)",
    "insert into dh141p values (1, 2), (3, 4)",
    "select pg_relation_size('dh141p1')",
    "checkpoint",
    "drop table dh141",
    "drop table dh141c",
    "drop table dh141p1",
    "drop table dh141p",
    "checkpoint",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgdhat141_", dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    log = os.path.join(data_dir, "dhat.log")
    proc = subprocess.Popen(
        [VG, "--tool=dhat", f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not start under dhat"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d

        def msg():
            t = rd(1)
            ln = struct.unpack("!i", rd(4))[0]
            return t, rd(ln - 4)

        while True:
            t, _ = msg()
            if t == b"Z":
                break

        def simple(q):
            qb = q.encode()
            s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z":
                    break

        for q in STMTS:
            simple(q)
        print("workload done")
        s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill(); proc.wait(timeout=30)
    txt = open(log).read()
    tot = re.search(r"Total:\s+([\d,]+) bytes", txt)
    peak = re.search(r"At t-gmax:\s+([\d,]+) bytes", txt)
    tend = re.search(r"At t-end:\s+([\d,]+) bytes", txt)
    print(f"DHAT total: {tot.group(1) if tot else '?'} bytes; "
          f"peak live: {peak.group(1) if peak else '?'}; "
          f"t-end: {tend.group(1) if tend else '?'}")
    # sanity: t-end should be a small fraction of total (no unbounded growth)
    if tot and tend:
        total = int(tot.group(1).replace(",", ""))
        tendb = int(tend.group(1).replace(",", ""))
        if total > 0 and tendb > total * 0.5:
            print("DHAT WARNING: t-end > 50% of total — possible unbounded growth")
            sys.exit(4)
    print("DHAT OK")


if __name__ == "__main__":
    main()
