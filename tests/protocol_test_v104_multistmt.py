#!/usr/bin/env python3
r"""v1.04 protocol tests: PG19 multi-statement simple-Query semantics.

Covers the v1.04 implicit-transaction block for multi-statement simple
protocol Queries (the psql `\;` work: psql sends `SELECT 1\; SELECT 2;`
as ONE Query message, backslashes removed), grounded in PostgreSQL
REL_19_STABLE (src/backend/tcop/postgres.c `exec_simple_query`):
a Query string with more than one statement runs inside an implicit
transaction block that commits when the string is exhausted.

- 2-3 SELECTs in one Q produce ordered, repeated result sets (T/D/C
  groups) followed by a single ReadyForQuery.
- Mixed SELECT/DDL/DML in one Q: every statement's set is returned.
- An error mid-string skips the remaining statements (one ErrorResponse,
  no further sets) and the implicit block is rolled back.
- Implicit-block rollback on error: writes AND schema changes made
  earlier in the same Q are undone (whole-query atomicity).
- COMMIT/ROLLBACK inside the block end it with WARNING "there is no
  transaction in progress" (NoticeResponse 'N', 01000); a fresh
  implicit block covers the rest of the string.
- VACUUM / SAVEPOINT / ROLLBACK TO / RELEASE inside the block are
  rejected with 25001, like PostgreSQL.
- BEGIN inside the block converts it to a regular explicit
  transaction (which then survives the end of the Q: ReadyForQuery 'T').
- COMMIT AND CHAIN / ROLLBACK AND CHAIN inside the block are 25001.
- One ReadyForQuery per Q with the correct transaction status
  ('I' idle, 'T' in-transaction, 'E' failed).

Self-starting: launches rustgres on 5450 with a fresh datadir.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5450
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
DATADIR = "/tmp/rg_proto_v104_multistmt"

def msg(t, payload):
    return t + struct.pack("!I", 4 + len(payload)) + payload

def cstr(s):
    return s.encode() + b"\x00"

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("closed")
        d += c
    return d

def read_msg(s):
    t = read_exact(s, 1)
    ln = struct.unpack("!I", read_exact(s, 4))[0]
    return t, read_exact(s, ln - 4)

def msg_fields(payload):
    i = 0
    fields = {}
    while i < len(payload) - 1:
        f = payload[i:i+1]
        e = payload.index(b"\x00", i + 1)
        fields[f] = payload[i+1:e].decode()
        i = e + 1
    return fields

class Conn:
    def __init__(self, port=PORT):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=15)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!I", 8 + len(params)) + b"\x00\x03\x00\x00" + params)
        while True:
            t, p = read_msg(self.s)
            if t == b"Z":
                break

    def q(self, sql):
        """Returns (sets, notices, ready_status). Each set is a dict with
        rows (list of lists), tag, err (sqlstate or None)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        sets = []
        cur = {"rows": [], "tag": None, "err": None}
        notices = []
        status = None
        while True:
            t, p = read_msg(self.s)
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos+4])
                    pos += 4
                    if ln == -1:
                        r.append(None)
                    else:
                        r.append(p[pos:pos+ln].decode())
                        pos += ln
                cur["rows"].append(r)
            elif t == b"C":
                cur["tag"] = p[:-1].decode()
                sets.append(cur)
                cur = {"rows": [], "tag": None, "err": None}
            elif t == b"E":
                cur["err"] = msg_fields(p).get(b"C")
            elif t == b"N":
                notices.append(msg_fields(p).get(b"M", ""))
            elif t == b"Z":
                if cur["rows"] or cur["tag"] or cur["err"]:
                    sets.append(cur)
                status = p[:1].decode()
                break
        return sets, notices, status

    def close(self):
        self.s.close()

PASS = 0
FAIL = 0

def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"PASS {name}")
    else:
        FAIL += 1
        print(f"FAIL {name} {detail}")

def start_server():
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", DATADIR],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(2)
    return proc

def stop_server(proc):
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    time.sleep(1)

def main():
    shutil.rmtree(DATADIR, ignore_errors=True)
    proc = start_server()
    try:
        c = Conn()

        # --- ordered repeated result sets ---
        sets, n, st = c.q("SELECT 1; SELECT 2; SELECT 3;")
        check("3 selects: 3 sets", len(sets) == 3, f"{len(sets)}")
        check("3 selects: ordered rows",
              [s["rows"] for s in sets] == [[["1"]], [["2"]], [["3"]]],
              f"{[s['rows'] for s in sets]}")
        check("3 selects: tags", [s["tag"] for s in sets] == ["SELECT 1"] * 3,
              f"{[s['tag'] for s in sets]}")
        check("3 selects: one ReadyForQuery, idle", st == "I", st)

        # --- mixed SELECT/DDL/DML ---
        sets, n, st = c.q(
            "CREATE TABLE ms104(a int); INSERT INTO ms104 VALUES (7); SELECT * FROM ms104;")
        check("mixed: 3 sets", len(sets) == 3, f"{len(sets)}")
        check("mixed: tags in order",
              [s["tag"] for s in sets] == ["CREATE TABLE", "INSERT 0 1", "SELECT 1"],
              f"{[s['tag'] for s in sets]}")
        check("mixed: select sees the insert", sets[2]["rows"] == [["7"]],
              f"{sets[2]['rows']}")
        check("mixed: idle at end", st == "I", st)

        # --- error in the middle skips the rest ---
        sets, n, st = c.q("SELECT 1; SELECT 1/0; SELECT 3;")
        check("mid-error: 2 sets, rest skipped", len(sets) == 2, f"{len(sets)}")
        check("mid-error: first ok", sets[0]["rows"] == [["1"]] and sets[0]["err"] is None,
              f"{sets[0]}")
        check("mid-error: 22012", sets[1]["err"] == "22012", f"{sets[1]['err']}")
        check("mid-error: ReadyForQuery idle (block aborted)", st == "I", st)

        # --- implicit block rolls back the whole Q on error ---
        c.q("CREATE TABLE rb104(a int); INSERT INTO rb104 VALUES (1); SELECT 1/0;")
        sets, n, st = c.q("SELECT * FROM rb104;")
        check("rollback: table creation undone", sets[0]["err"] == "42P01",
              f"{sets[0]['err']}")

        # --- COMMIT inside the block warns; rest runs in a fresh block ---
        sets, n, st = c.q("SELECT 1; COMMIT; SELECT 2;")
        check("commit-in-block: 3 sets", len(sets) == 3, f"{len(sets)}")
        check("commit-in-block: warning",
              n == ["there is no transaction in progress"], f"{n}")
        check("commit-in-block: tags",
              [s["tag"] for s in sets] == ["SELECT 1", "COMMIT", "SELECT 1"],
              f"{[s['tag'] for s in sets]}")
        check("commit-in-block: idle", st == "I", st)

        # --- ROLLBACK inside the block warns; rest runs in a fresh block ---
        sets, n, st = c.q(
            "CREATE TABLE rb2_104(a int); INSERT INTO rb2_104 VALUES (9); "
            "ROLLBACK; SELECT * FROM rb2_104;")
        check("rollback-in-block: warning",
              n == ["there is no transaction in progress"], f"{n}")
        check("rollback-in-block: 4 sets", len(sets) == 4, f"{len(sets)}")
        check("rollback-in-block: insert undone by ROLLBACK",
              sets[3]["err"] == "42P01", f"{sets[3]}")

        # --- VACUUM / savepoint commands rejected in the block ---
        sets, n, st = c.q("SELECT 1; VACUUM;")
        check("vacuum-in-block: 25001", sets[1]["err"] == "25001",
              f"{sets[1]['err']}")
        sets, n, st = c.q("SELECT 1; SAVEPOINT sp104;")
        check("savepoint-in-block: 25001", sets[1]["err"] == "25001",
              f"{sets[1]['err']}")
        sets, n, st = c.q("SELECT 1; ROLLBACK TO sp104;")
        check("rollback-to-in-block: 25001", sets[1]["err"] == "25001",
              f"{sets[1]['err']}")
        sets, n, st = c.q("SELECT 1; RELEASE sp104;")
        check("release-in-block: 25001", sets[1]["err"] == "25001",
              f"{sets[1]['err']}")

        # --- BEGIN converts the implicit block to an explicit txn ---
        sets, n, st = c.q("SELECT 1; BEGIN; SELECT 2;")
        check("begin-converts: 3 sets", len(sets) == 3, f"{len(sets)}")
        check("begin-converts: still in txn after Q", st == "T", st)
        sets, n, st = c.q("COMMIT;")
        check("begin-converts: commit ends it", st == "I", st)

        # --- AND CHAIN inside the block is 25001 ---
        sets, n, st = c.q("SELECT 1; COMMIT AND CHAIN;")
        check("commit-and-chain: 25001", sets[1]["err"] == "25001",
              f"{sets[1]['err']}")
        sets, n, st = c.q("SELECT 1; ROLLBACK AND CHAIN;")
        check("rollback-and-chain: 25001", sets[1]["err"] == "25001",
              f"{sets[1]['err']}")

        # --- writes before an in-Q COMMIT survive; after-Q state is idle ---
        sets, n, st = c.q(
            "CREATE TABLE kc104(a int); INSERT INTO kc104 VALUES (5); "
            "COMMIT; SELECT * FROM kc104;")
        check("in-q-commit: insert kept", sets[3]["rows"] == [["5"]],
              f"{sets[3]['rows']}")
        c2 = Conn()
        sets2, _, _ = c2.q("SELECT * FROM kc104;")
        check("in-q-commit: visible to a new connection",
              sets2[0]["rows"] == [["5"]], f"{sets2[0]['rows']}")
        c2.close()

        # --- explicit transactions still behave (regression) ---
        sets, n, st = c.q("BEGIN; SELECT 1; COMMIT;")
        check("explicit: tags", [s["tag"] for s in sets] == ["BEGIN", "SELECT 1", "COMMIT"],
              f"{[s['tag'] for s in sets]}")
        check("explicit: idle", st == "I", st)

        # --- single statement: plain autocommit (regression) ---
        sets, n, st = c.q("SELECT 42;")
        check("single: one set", len(sets) == 1 and sets[0]["rows"] == [["42"]],
              f"{sets}")
        check("single: idle", st == "I", st)

        c.close()
    finally:
        stop_server(proc)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)

if __name__ == "__main__":
    main()
