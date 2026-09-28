#!/usr/bin/env python3
"""memcheck_v118.py — valgrind memcheck for the v1.18 temp index-order paths.

Two-phase (v0.94 driver pattern):
  phase 1: fresh datadir under valgrind; temp table + temp index, then
           ORDER BY ASC/DESC/LIMIT/NULLS FIRST through the temp-aware
           OrderHint path, plus a permanent-table control.
  phase 2: restart on the same datadir under valgrind (WAL replay);
           temp objects are session-local (gone), permanent control
           persists; workload re-runs.

NOTE: temp tables are session-local, so the workload holds ONE
connection open for the whole phase (unlike the v117 driver, which
reconnected per statement).

Errors-for-leak-kinds=none (memory errors only); --error-exitcode=99.
"""
import os, socket, struct, subprocess, sys, tempfile, time
import re as _re

VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")
PORT = 5619

WORKLOAD = [
    ("ddl", "CREATE TEMP TABLE mc118t (f1 int);", None),
    ("dml", "INSERT INTO mc118t VALUES (42),(3),(10),(7),(null),(null),(1);", None),
    ("ddl", "CREATE INDEX mc118i ON mc118t (f1);", None),
    # the v1.18 conformance shape (was a server panic before the fix)
    ("q", "SELECT * FROM mc118t ORDER BY f1;", None),
    ("q", "SELECT * FROM mc118t ORDER BY f1 DESC;", None),
    ("q", "SELECT * FROM mc118t ORDER BY f1 LIMIT 3;", None),
    ("q", "SELECT * FROM mc118t ORDER BY f1 NULLS FIRST;", None),
    ("q", "SELECT * FROM mc118t ORDER BY f1 DESC NULLS LAST;", None),
    # DESC temp index
    ("ddl", "DROP INDEX mc118i;", None),
    ("ddl", "CREATE INDEX mc118d ON mc118t (f1 DESC);", None),
    ("q", "SELECT * FROM mc118t ORDER BY f1 DESC;", None),
    ("q", "SELECT * FROM mc118t ORDER BY f1;", None),
    # permanent-table control (global index map)
    ("ddl", "CREATE TABLE mc118p (f1 int);", None),
    ("dml", "INSERT INTO mc118p VALUES (5),(null),(2);", None),
    ("ddl", "CREATE INDEX mc118pi ON mc118p (f1);", None),
    ("q", "SELECT * FROM mc118p ORDER BY f1;", None),
    ("q", "SELECT * FROM mc118p ORDER BY f1 DESC;", None),
    ("ddl", "DROP TABLE mc118p;", None),
]


class Session:
    """One persistent wire-protocol connection (temp tables survive)."""
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
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
                raise RuntimeError("closed")
            d += c
        return d

    def _msg(self):
        t = self._rd(1)
        ln = struct.unpack("!i", self._rd(4))[0]
        return t, self._rd(ln - 4)

    def q(self, sql):
        self.s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        codes = []
        while True:
            t, p = self._msg()
            if t == b"E":
                m = _re.search(rb"C([0-9A-Z]{5})", p)
                codes.append(m.group(1).decode() if m else "?????")
            elif t == b"Z":
                break
        return codes

    def close(self):
        self.s.close()


def run_workload(nfail):
    sess = Session()
    try:
        for name, sql, want in WORKLOAD:
            codes = sess.q(sql)
            if want is None:
                if codes:
                    print(f"UNEXPECTED-ERR {name}: {codes} :: {sql[:80]}")
                    nfail[0] += 1
            else:
                if codes != [want]:
                    print(f"WRONG-ERR {name}: got {codes} want [{want}] :: {sql[:80]}")
                    nfail[0] += 1
    finally:
        sess.close()


def wait_for_port():
    for _ in range(900):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.5)
    return False


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc118_",
                                dir=os.path.expanduser("~/workspace/.tmp-cargo"))
    nfail = [0]

    for phase, logname in [(1, "vg.log"), (2, "vg2.log")]:
        log = os.path.join(data_dir, logname)
        proc = subprocess.Popen(
            [VG, "--tool=memcheck", "--error-exitcode=99",
             "--errors-for-leak-kinds=none",
             f"--log-file={log}",
             BIN, "--data-dir", data_dir, "--port", str(PORT)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            if not wait_for_port():
                print(f"phase {phase}: server did not start under valgrind")
                proc.kill()
                sys.exit(2)
            run_workload(nfail)
            print(f"phase {phase}: {nfail[0]} workload failures (cumulative)")
        finally:
            proc.terminate()
            proc.wait(timeout=120)
        # phase 2 restarts on the same datadir (WAL replay); loop handles it

    print(f"VALGRIND-LOGS {os.path.join(data_dir, 'vg.log')} {os.path.join(data_dir, 'vg2.log')}")
    print(f"WORKLOAD-FAILURES {nfail[0]}")


if __name__ == "__main__":
    main()
