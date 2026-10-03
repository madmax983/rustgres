#!/usr/bin/env python3
"""memcheck_v142.py — run the v1.42 feature paths under valgrind memcheck.

Exercises: pg_class.relkind='t' for TOAST tables (PG19
RELKIND_TOASTVALUE), 'r' for ordinary tables, 'p' for partitioned
tables; toast table creation + pg_class scans; reltoastrelid::regclass
round-trip.

Uses the DEBUG binary.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5542
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

STMTS = [
    # toast table relkind='t'
    "create table mc142a (f1 text, f2 text)",
    "insert into mc142a values (repeat('x', 5000), repeat('y', 100))",
    "select relname, relkind from pg_class where relname like 'pg_toast.%'",
    "select relname, relkind from pg_class where relname = 'mc142a'",
    # ordinary table relkind='r'
    "create table mc142b (a int)",
    "select relkind from pg_class where relname = 'mc142b'",
    # partitioned table relkind='p'
    "create table mc142p (a int, b int) partition by range (a, b)",
    "select relkind from pg_class where relname = 'mc142p'",
    # reltoastrelid::regclass round-trip (the \gset query shape)
    "select reltoastrelid::regclass as reltoastname from pg_class where oid = 'mc142a'::regclass",
    # toast table's own reltoastrelid is 0
    "select reltoastrelid from pg_class where relname like 'pg_toast.%'",
    # wider pg_class scan
    "select oid, relname, reltoastrelid, relkind from pg_class order by relname",
]


def q(s, sql):
    qb = sql.encode()
    s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
    # read until ReadyForQuery
    while True:
        t = s.recv(1)
        if not t:
            raise RuntimeError("connection closed")
        (ln,) = struct.unpack("!i", s.recv(4))
        body = s.recv(ln - 4)
        while len(body) < ln - 4:
            body += s.recv(ln - 4 - len(body))
        if t == b"Z":
            return
        if t == b"E":
            print("ERROR on %r" % sql[:60])


def main():
    if not os.path.exists(BIN):
        print("missing debug binary; run cargo build first")
        return 2
    data_dir = tempfile.mkdtemp(prefix="rg_v142_vg_")
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--leak-check=full",
         "--errors-for-leak-kinds=none",
         "--log-file=" + log,
         BIN, "--port", str(PORT), "--data-dir", data_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(300):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not start")
            return 2
        # startup
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t = s.recv(1)
            (ln,) = struct.unpack("!i", s.recv(4))
            s.recv(ln - 4)
            if t == b"Z":
                break
        for sql in STMTS:
            q(s, sql)
        s.close()
    finally:
        proc.terminate()
        proc.wait()
    # parse the valgrind log
    err = lost = invalid = 0
    with open(log) as f:
        for line in f:
            if "ERROR SUMMARY" in line:
                err = int(line.split(":")[1].split()[0])
            if "definitely lost:" in line:
                lost = int(line.split("definitely lost:")[1].split()[0].replace(",", ""))
            if "Invalid read" in line or "Invalid write" in line:
                invalid += 1
    print("valgrind: ERROR SUMMARY=%d definitely-lost=%d invalid-access=%d" % (err, lost, invalid))
    print("log: %s" % log)
    return 0 if (err == 0 and lost == 0 and invalid == 0) else 1


if __name__ == "__main__":
    sys.exit(main())
