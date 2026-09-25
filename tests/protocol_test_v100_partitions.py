#!/usr/bin/env python3
r"""v1.00 protocol tests: declarative partitioning end-to-end.

Covers the v1.00 partition iteration, grounded in PostgreSQL REL_19_STABLE
(src/backend/partitioning/partdesc.c, src/backend/executor/execPartition.c):

- RANGE / LIST / HASH PARTITION BY with routing to the correct leaf.
- PARTITION OF (declarative partition creation).
- ALTER TABLE ... ATTACH PARTITION.
- Multilevel (sub-partitioned) routing.
- DEFAULT partitions.
- Expression-key partitions (e.g. PARTITION BY RANGE ((a + b))).
- Partition + BEFORE INSERT trigger interaction: leaf triggers fire during
  routing (the three insert-suite REAL-FAILs: mlparted5, brtrigpartcon,
  brtrigpartcon1).
- pg_class.relkind = 'p' for partitioned tables, 'r' for leaves/ordinary.
- Overlap errors (23514), no-partition-found (23514), direct-leaf
  constraint violations (23514).
- Restart durability: partitions and routed rows survive WAL replay and
  checkpoint/restart.

Self-starting: launches rustgres on 5446 with a fresh datadir, restarts
it mid-run to exercise WAL replay and checkpoint recovery.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5446
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
DATADIR = "/tmp/rg_proto_v100_part"

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
        """Returns (rows, err_code). rows is list of lists (str or None)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        rows, err_code = [], None
        while True:
            t, p = read_msg(self.s)
            if t == b"T":
                pass
            elif t == b"D":
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
                rows.append(r)
            elif t == b"E":
                i = 0
                while i < len(p) - 1:
                    f = p[i:i+1]
                    e = p.index(b"\x00", i+1)
                    if f == b"C":
                        err_code = p[i+1:e].decode()
                    i = e + 1
            elif t == b"Z":
                break
        return rows, err_code

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

        # --- RANGE partitioning ---
        r, e = c.q("CREATE TABLE rp (a INT, b TEXT) PARTITION BY RANGE (a);")
        check("range: create partitioned", e is None, e)
        r, e = c.q("CREATE TABLE rp1 PARTITION OF rp FOR VALUES FROM (1) TO (10);")
        check("range: partition of", e is None, e)
        r, e = c.q("CREATE TABLE rp2 PARTITION OF rp FOR VALUES FROM (10) TO (20);")
        check("range: partition of 2", e is None, e)
        r, e = c.q("INSERT INTO rp VALUES (5, 'five'), (15, 'fifteen');")
        check("range: routed insert", e is None, e)
        r, e = c.q("SELECT a, b FROM rp ORDER BY a;")
        check("range: parent select sees routed rows", r == [['5', 'five'], ['15', 'fifteen']], f"{r} {e}")
        r, e = c.q("SELECT a FROM rp1;")
        check("range: leaf1 has its row", r == [['5']], f"{r} {e}")
        r, e = c.q("SELECT a FROM rp2;")
        check("range: leaf2 has its row", r == [['15']], f"{r} {e}")

        # --- Overlap detection (23514) ---
        r, e = c.q("CREATE TABLE rp_overlap PARTITION OF rp FOR VALUES FROM (5) TO (15);")
        check("range: overlap rejected 23514", e == "23514", e)

        # --- No partition found (23514) ---
        r, e = c.q("INSERT INTO rp VALUES (99, 'nowhere');")
        check("range: no partition 23514", e == "23514", e)

        # --- Direct leaf insert violating bound (23514) ---
        r, e = c.q("INSERT INTO rp1 VALUES (50, 'wrong');")
        check("range: direct leaf violation 23514", e == "23514", e)

        # --- LIST partitioning ---
        r, e = c.q("CREATE TABLE lp (a INT, b TEXT) PARTITION BY LIST (b);")
        check("list: create", e is None, e)
        r, e = c.q("CREATE TABLE lp_a PARTITION OF lp FOR VALUES IN ('a', 'b');")
        check("list: partition of", e is None, e)
        r, e = c.q("CREATE TABLE lp_c PARTITION OF lp FOR VALUES IN ('c');")
        check("list: partition of 2", e is None, e)
        r, e = c.q("INSERT INTO lp VALUES (1, 'a'), (2, 'c');")
        check("list: routed insert", e is None, e)
        r, e = c.q("SELECT b FROM lp_a;")
        check("list: leaf a", r == [['a']], f"{r} {e}")

        # --- HASH partitioning ---
        r, e = c.q("CREATE TABLE hp (a INT) PARTITION BY HASH (a);")
        check("hash: create", e is None, e)
        r, e = c.q("CREATE TABLE hp0 PARTITION OF hp FOR VALUES WITH (MODULUS 2, REMAINDER 0);")
        check("hash: partition 0", e is None, e)
        r, e = c.q("CREATE TABLE hp1 PARTITION OF hp FOR VALUES WITH (MODULUS 2, REMAINDER 1);")
        check("hash: partition 1", e is None, e)
        r, e = c.q("INSERT INTO hp SELECT generate_series(1, 20);")
        check("hash: routed insert", e is None, e)
        r, e = c.q("SELECT count(*) FROM hp;")
        check("hash: all rows visible", r == [['20']], f"{r} {e}")
        r, e = c.q("SELECT (SELECT count(*) FROM hp0) + (SELECT count(*) FROM hp1);")
        check("hash: rows split across leaves", r == [['20']], f"{r} {e}")

        # --- ATTACH PARTITION ---
        r, e = c.q("CREATE TABLE ap (a INT) PARTITION BY RANGE (a);")
        check("attach: create parent", e is None, e)
        r, e = c.q("CREATE TABLE ap1 (a INT);")
        check("attach: create standalone", e is None, e)
        r, e = c.q("ALTER TABLE ap ATTACH PARTITION ap1 FOR VALUES FROM (1) TO (5);")
        check("attach: attach partition", e is None, e)
        r, e = c.q("INSERT INTO ap VALUES (3);")
        check("attach: routed insert", e is None, e)
        r, e = c.q("SELECT a FROM ap1;")
        check("attach: leaf has row", r == [['3']], f"{r} {e}")

        # --- Multilevel (sub-partitioning) ---
        r, e = c.q("CREATE TABLE mp (a INT, b INT) PARTITION BY RANGE (a);")
        check("multi: create root", e is None, e)
        r, e = c.q("CREATE TABLE mp1 PARTITION OF mp FOR VALUES FROM (1) TO (100) PARTITION BY RANGE (b);")
        check("multi: create intermediate", e is None, e)
        r, e = c.q("CREATE TABLE mp1x PARTITION OF mp1 FOR VALUES FROM (1) TO (10);")
        check("multi: create leaf", e is None, e)
        r, e = c.q("INSERT INTO mp VALUES (50, 5);")
        check("multi: routed to sub-leaf", e is None, e)
        r, e = c.q("SELECT a, b FROM mp1x;")
        check("multi: sub-leaf has row", r == [['50', '5']], f"{r} {e}")

        # --- DEFAULT partition ---
        r, e = c.q("CREATE TABLE dp (a INT) PARTITION BY LIST (a);")
        check("default: create", e is None, e)
        r, e = c.q("CREATE TABLE dp1 PARTITION OF dp FOR VALUES IN (1);")
        check("default: partition", e is None, e)
        r, e = c.q("CREATE TABLE dpdef PARTITION OF dp DEFAULT;")
        check("default: default partition", e is None, e)
        r, e = c.q("INSERT INTO dp VALUES (1), (999);")
        check("default: routed insert", e is None, e)
        r, e = c.q("SELECT a FROM dpdef;")
        check("default: default leaf catches rest", r == [['999']], f"{r} {e}")

        # --- Expression-key partition ---
        r, e = c.q("CREATE TABLE ep (a INT, b INT) PARTITION BY RANGE ((a + b));")
        check("exprkey: create", e is None, e)
        r, e = c.q("CREATE TABLE ep1 PARTITION OF ep FOR VALUES FROM (1) TO (10);")
        check("exprkey: partition", e is None, e)
        r, e = c.q("INSERT INTO ep VALUES (3, 4);")
        check("exprkey: routed insert", e is None, e)
        r, e = c.q("SELECT a, b FROM ep1;")
        check("exprkey: leaf has row", r == [['3', '4']], f"{r} {e}")

        # --- pg_class.relkind ---
        r, e = c.q("SELECT relkind FROM pg_class WHERE relname = 'rp';")
        check("relkind: partitioned is 'p'", r == [['p']], f"{r} {e}")
        r, e = c.q("SELECT relkind FROM pg_class WHERE relname = 'rp1';")
        check("relkind: leaf is 'r'", r == [['r']], f"{r} {e}")
        r, e = c.q("SELECT relkind FROM pg_class WHERE relname = 'mp1';")
        check("relkind: intermediate is 'p'", r == [['p']], f"{r} {e}")

        # --- Partition + BEFORE trigger interaction (insert-suite REAL-FAILs) ---
        r, e = c.q("CREATE TABLE mlparted5 (a INT, b INT, c TEXT) PARTITION BY RANGE (a);")
        check("trig: create mlparted5", e is None, e)
        r, e = c.q("CREATE TABLE mlparted5a PARTITION OF mlparted5 FOR VALUES FROM (1) TO (100);")
        check("trig: create leaf", e is None, e)
        r, e = c.q("""CREATE FUNCTION mlparted5abrtrig_func() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN new.c = 'b'; RETURN new; END; $$;""")
        check("trig: create func", e is None, e)
        r, e = c.q("CREATE TRIGGER mlparted5abrtrig BEFORE INSERT ON mlparted5a FOR EACH ROW EXECUTE FUNCTION mlparted5abrtrig_func();")
        check("trig: create trigger", e is None, e)
        r, e = c.q("INSERT INTO mlparted5 (a, b, c) VALUES (1, 40, 'a');")
        check("trig: REAL-FAIL1 insert", e is None, e)
        r, e = c.q("SELECT c FROM mlparted5a;")
        check("trig: trigger modified row", r == [['b']], f"{r} {e}")

        r, e = c.q("CREATE TABLE brtrigpartcon (a INT, b TEXT) PARTITION BY RANGE (a);")
        check("trig: create brtrigpartcon", e is None, e)
        r, e = c.q("CREATE TABLE brtrigpartcon1 PARTITION OF brtrigpartcon FOR VALUES FROM (1) TO (100);")
        check("trig: create leaf", e is None, e)
        r, e = c.q("""CREATE FUNCTION brtrigpartcon1trigf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN new.a := 2; RETURN new; END; $$;""")
        check("trig: create func2", e is None, e)
        r, e = c.q("CREATE TRIGGER brtrigpartcon1trig BEFORE INSERT ON brtrigpartcon1 FOR EACH ROW EXECUTE FUNCTION brtrigpartcon1trigf();")
        check("trig: create trigger2", e is None, e)
        r, e = c.q("INSERT INTO brtrigpartcon VALUES (1, 'hi there');")
        check("trig: REAL-FAIL2 insert", e is None, e)
        r, e = c.q("INSERT INTO brtrigpartcon1 VALUES (1, 'hi there');")
        check("trig: REAL-FAIL3 insert", e is None, e)
        r, e = c.q("SELECT a FROM brtrigpartcon1 ORDER BY a;")
        check("trig: trigger modified a to 2", r == [['2'], ['2']], f"{r} {e}")

        # --- Restart durability ---
        c.q("CHECKPOINT;")
        c.close()
        stop_server(proc)
        proc = start_server()
        c = Conn()
        r, e = c.q("SELECT count(*) FROM rp;")
        check("restart: rp rows survive", r == [['2']], f"{r} {e}")
        r, e = c.q("SELECT count(*) FROM hp;")
        check("restart: hp rows survive", r == [['20']], f"{r} {e}")
        r, e = c.q("SELECT relkind FROM pg_class WHERE relname = 'rp';")
        check("restart: relkind survives", r == [['p']], f"{r} {e}")
        r, e = c.q("INSERT INTO rp VALUES (7, 'seven');")
        check("restart: routing works after restart", e is None, e)

        c.close()
    finally:
        stop_server(proc)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)

if __name__ == "__main__":
    main()
