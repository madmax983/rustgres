#!/usr/bin/env python3
"""dhat_v142.py — DHAT heap profile for the v1.42 feature paths.

Workload: pg_class scans over toast/ordinary/partitioned tables
(relkind derivation), toast table creation, reltoastrelid::regclass
round-trips. Uses the DEBUG binary.
"""
import os, re, socket, struct, subprocess, sys, tempfile, time

PORT = 5592
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    "create table dh142(a int, b text, c text)",
    "insert into dh142 select g, repeat('v', 100), repeat('w', 5000) from generate_series(1, 50) g",
    # pg_class scans over toast/ordinary tables (relkind derivation)
    "select relname, relkind from pg_class order by relname",
    "select relname, relkind from pg_class where relname like 'pg_toast.%'",
    "select reltoastrelid::regclass as t from pg_class where oid = 'dh142'::regclass",
    # partitioned table relkind
    "create table dh142p (a int, b int) partition by range (a)",
    "create table dh142p1 partition of dh142p for values from (1) to (100)",
    "insert into dh142p values (1, 2), (3, 4)",
    "select relname, relkind from pg_class where relname like 'dh142%'",
    "checkpoint",
    "drop table dh142",
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
        s = socket.create_connection(("127.0.0.1", PORT), timeout=600)
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
