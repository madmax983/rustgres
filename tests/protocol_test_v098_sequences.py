#!/usr/bin/env python3
r"""v0.98 protocol tests: PostgreSQL 19 sequence parity.

Covers the v0.98 behavior changes, grounded in PostgreSQL REL_19_STABLE
sequence semantics (src/backend/commands/sequence.c):

- volatile quals are NOT pushed down past subquery boundaries: the
  subselect.sql regression case `... where ten < 10 + nextval('ts1')`
  must advance the sequence once per surviving row (final nextval = 11,
  not 21). Same for a volatile UDF (tattle): exactly one NOTICE per
  output row, never evaluated twice.
- currval: 55000 before any nextval in the session; session-local.
- lastval: 55000 before any nextval; tracks the most recent nextval
  across sequences; setval never touches it.
- setval(n, true)/(n, false) semantics; setval outside [min,max] ->
  22003 ("setval: value n is out of bounds for sequence ...").
- ALTER SEQUENCE ... RESTART WITH n outside [min,max] -> 22023.
- CREATE/ALTER SEQUENCE ... CACHE n (metadata); CACHE < 1 -> 22023.
- OWNED BY table.col / OWNED BY NONE; DROP TABLE drops the owned
  sequence; invalid OWNED BY targets error and leak nothing.
- ALTER SEQUENCE IF EXISTS on a missing sequence is a no-op.
- pg_sequences and information_schema.sequences virtual catalogs.

Self-starting: launches rustgres on 5444 with a fresh datadir.
"""
import socket, struct, subprocess, sys, time, os

PORT = 5444
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")

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
        """Returns (rows, err_code, notices). rows is list of lists (str or None)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        rows, err_code, notices = [], None, []
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
            elif t == b"N":
                i = 0
                fields = {}
                while i < len(p) - 1:
                    f = p[i:i+1]
                    e = p.index(b"\x00", i+1)
                    fields[f] = p[i+1:e].decode()
                    i = e + 1
                notices.append(fields.get(b"M", ""))
            elif t == b"Z":
                break
        return rows, err_code, notices

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

def main():
    global PASS, FAIL
    datadir = "/tmp/rg_proto_v098"
    os.system(f"rm -rf {datadir}")
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", datadir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(2)
    try:
        c = Conn()
        c2 = Conn()  # second session for isolation checks

        # --- setup ---
        rows, err, _ = c.q("create table v98t as select g % 10 as ten from generate_series(1, 100) g;")
        assert err is None, f"setup v98t: {err}"
        rows, err, _ = c.q("create temp sequence ts1;")
        assert err is None, f"setup ts1: {err}"

        # --- 1. volatile pushdown: the exact subselect.sql regression ---
        rows, err, _ = c.q(
            "select * from (select distinct ten from v98t) ss "
            "where ten < 10 + nextval('ts1') order by 1;")
        check("V1 pushdown rows", rows == [[str(i)] for i in range(10)], f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('ts1');")
        check("V1 nextval==11", rows == [["11"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select currval('ts1');")
        check("V1 currval==11", rows == [["11"]], f"rows={rows} err={err}")

        # --- 2. currval before first nextval -> 55000, session-local ---
        rows, err, _ = c.q("create sequence v98_cur;")
        assert err is None
        rows, err, _ = c.q("select currval('v98_cur');")
        check("C1 currval 55000", err == "55000", f"rows={rows} err={err}")
        rows, err, _ = c2.q("select currval('v98_cur');")
        check("C2 currval session-local", err == "55000", f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_cur');")
        assert err is None and rows == [["1"]]
        rows, err, _ = c2.q("select currval('v98_cur');")
        check("C3 other session still 55000", err == "55000", f"rows={rows} err={err}")
        rows, err, _ = c.q("select currval('v98_cur');")
        check("C4 own session sees 1", rows == [["1"]], f"rows={rows} err={err}")

        # --- 3. lastval ---
        rows, err, _ = c2.q("select lastval();")
        check("L1 lastval 55000", err == "55000", f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_a start 100;")
        assert err is None, f"v98_a: {err}"
        rows, err, _ = c.q("create sequence v98_b start 200;")
        assert err is None, f"v98_b: {err}"
        rows, err, _ = c.q("select nextval('v98_a');")
        assert rows == [["100"]]
        rows, err, _ = c.q("select lastval();")
        check("L2 lastval tracks a", rows == [["100"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_b');")
        assert rows == [["200"]]
        rows, err, _ = c.q("select lastval();")
        check("L3 lastval follows most recent", rows == [["200"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select setval('v98_a', 500);")
        assert err is None
        rows, err, _ = c.q("select lastval();")
        check("L4 setval does not touch lastval", rows == [["200"]], f"rows={rows} err={err}")

        # --- 4. setval semantics + bounds ---
        rows, err, _ = c.q("create sequence v98_sv start 1;")
        assert err is None
        rows, err, _ = c.q("select setval('v98_sv', 41);")
        check("S1 setval true returns n", rows == [["41"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_sv');")
        check("S2 next after setval(n,true) = n+inc", rows == [["42"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select setval('v98_sv', 77, false);")
        check("S3 setval false returns n", rows == [["77"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_sv');")
        check("S4 next after setval(n,false) = n", rows == [["77"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select currval('v98_sv');")
        check("S5 currval tracks setval(n,true)", rows == [["77"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_bd minvalue 1 maxvalue 10 start 1;")
        assert err is None
        rows, err, _ = c.q("select setval('v98_bd', 99);")
        check("S6 setval above max -> 22003", err == "22003", f"rows={rows} err={err}")
        rows, err, _ = c.q("select setval('v98_bd', 0);")
        check("S7 setval below min -> 22003", err == "22003", f"rows={rows} err={err}")
        rows, err, _ = c.q("select setval('v98_bd', 10);")
        check("S8 setval at max ok", rows == [["10"]] and err is None, f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_cf start 1;")
        assert err is None
        rows, err, _ = c.q("select nextval('v98_cf');")
        assert rows == [["1"]]
        rows, err, _ = c.q("select setval('v98_cf', 77, false);")
        assert err is None
        rows, err, _ = c.q("select currval('v98_cf');")
        check("S9 setval(n,false) leaves defined currval", rows == [["1"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_cu start 1;")
        assert err is None
        rows, err, _ = c.q("select setval('v98_cu', 77, false);")
        assert err is None
        rows, err, _ = c.q("select currval('v98_cu');")
        check("S10 setval(n,false) leaves undefined currval", err == "55000", f"rows={rows} err={err}")

        # --- 5. RESTART bounds ---
        rows, err, _ = c.q("alter sequence v98_bd restart with 99;")
        check("R1 restart above max -> 22023", err == "22023", f"rows={rows} err={err}")
        rows, err, _ = c.q("alter sequence v98_bd restart with 0;")
        check("R2 restart below min -> 22023", err == "22023", f"rows={rows} err={err}")
        rows, err, _ = c.q("alter sequence v98_bd restart with 5;")
        check("R3 restart in range ok", err is None, f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_bd');")
        check("R4 restart(n) next returns n", rows == [["5"]], f"rows={rows} err={err}")

        # --- 6. CACHE ---
        rows, err, _ = c.q("create sequence v98_cache cache 20;")
        check("K1 create cache 20", err is None, f"err={err}")
        rows, err, _ = c.q("select cache_size from pg_sequences where sequencename='v98_cache';")
        check("K2 catalog shows 20", rows == [["20"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("alter sequence v98_cache cache 50;")
        check("K3 alter cache 50", err is None, f"err={err}")
        rows, err, _ = c.q("select cache_size from pg_sequences where sequencename='v98_cache';")
        check("K4 catalog shows 50", rows == [["50"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_cache0 cache 0;")
        check("K5 cache 0 -> 22023", err == "22023", f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_cache_neg cache -5;")
        check("K6 cache -5 -> 22023", err == "22023", f"rows={rows} err={err}")

        # --- 7. OWNED BY ---
        rows, err, _ = c.q("create table v98_own_t(a int);")
        assert err is None
        rows, err, _ = c.q("create sequence v98_own owned by v98_own_t.a;")
        check("O1 create owned by", err is None, f"err={err}")
        rows, err, _ = c.q("drop table v98_own_t;")
        check("O2 drop table ok", err is None, f"err={err}")
        rows, err, _ = c.q("select nextval('v98_own');")
        check("O3 owned seq dropped with table", err == "42P01", f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_ownbad owned by nosuch_t.a;")
        check("O4 bad table errors", err is not None, f"err={err}")
        rows, err, _ = c.q("select sequencename from pg_sequences where sequencename='v98_ownbad';")
        check("O5 bad owned-by leaks nothing", rows == [], f"rows={rows} err={err}")
        rows, err, _ = c.q("create table v98_own_t2(a int, b int);")
        assert err is None, f"v98_own_t2: {err}"
        rows, err, _ = c.q("create sequence v98_own2;")
        assert err is None, f"v98_own2: {err}"
        rows, err, _ = c.q("alter sequence v98_own2 owned by v98_own_t2.b;")
        check("O6 alter owned by", err is None, f"err={err}")
        rows, err, _ = c.q("alter sequence v98_own2 owned by none;")
        check("O7 alter owned by none", err is None, f"err={err}")
        rows, err, _ = c.q("drop table v98_own_t2;")
        assert err is None
        rows, err, _ = c.q("select nextval('v98_own2');")
        check("O8 unowned seq survives drop", rows == [["1"]], f"rows={rows} err={err}")

        # --- 8. IF EXISTS / catalogs ---
        rows, err, _ = c.q("alter sequence if exists v98_nosuch restart;")
        check("I1 alter if exists no-op", err is None, f"err={err}")
        rows, err, _ = c.q("alter sequence v98_nosuch restart;")
        check("I2 alter missing errors", err == "42P01", f"err={err}")
        rows, err, _ = c.q(
            "select schemaname, sequencename, sequenceowner, data_type, start_value, "
            "min_value, max_value, increment_by, cycle, cache_size, last_value "
            "from pg_sequences where sequencename='v98_cache';")
        check("I3 pg_sequences full row",
              rows == [["public", "v98_cache", "postgres", "bigint", "1", "1",
                        "9223372036854775807", "1", "f", "50", None]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q(
            "select sequence_catalog, sequence_schema, sequence_name, data_type, "
            "numeric_precision, numeric_precision_radix, numeric_scale, start_value, "
            "minimum_value, maximum_value, increment, cycle_option "
            "from information_schema.sequences where sequence_name='v98_cache';")
        check("I4 information_schema.sequences row",
              len(rows) == 1 and rows[0][2] == "v98_cache" and rows[0][3] == "bigint"
              and rows[0][4] == "64" and rows[0][5] == "2" and rows[0][11] == "NO",
              f"rows={rows} err={err}")

        # --- 9. volatile UDF: evaluated once per surviving row, never doubled ---
        # (a SQL-language VOLATILE function wrapping nextval makes the
        # double-evaluation observable; plpgsql here is a bounded
        # single-RETURN subset, so the conformance tattle body is 0A000)
        rows, err, _ = c.q("create sequence v98_nvs;")
        assert err is None, f"v98_nvs: {err}"
        rows, err, _ = c.q(
            "create function v98_nv() returns bigint volatile language sql "
            "as $$ select nextval('v98_nvs') $$;")
        assert err is None, f"v98_nv create: {err}"
        rows, err, _ = c.q(
            "select * from (select distinct ten from v98t) ss "
            "where ten < 10 + v98_nv() order by 1;")
        check("U1 volatile-UDF rows", rows == [[str(i)] for i in range(10)],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_nvs');")
        check("U2 volatile UDF evaluated exactly once per row",
              rows == [["11"]], f"rows={rows} err={err}")

        # --- 10. serial regression ---
        rows, err, _ = c.q("create table v98_ser(id serial primary key, v text);")
        assert err is None
        rows, err, _ = c.q("insert into v98_ser(v) values ('a'),('b') returning id;")
        check("Z1 serial still works", rows == [["1"], ["2"]], f"rows={rows} err={err}")

        # --- 11. DROP COLUMN drops the sequences owned by that column only ---
        rows, err, _ = c.q("create table v98_dco(a int, b int);")
        assert err is None
        rows, err, _ = c.q("create sequence v98_dca owned by v98_dco.a;")
        assert err is None
        rows, err, _ = c.q("create sequence v98_dcb owned by v98_dco.b;")
        assert err is None
        rows, err, _ = c.q("alter table v98_dco drop column a;")
        assert err is None, f"drop column a: {err}"
        rows, err, _ = c.q("select nextval('v98_dca');")
        check("Z2 dropping the column drops its owned sequence",
              err is not None and "42P01" in err, f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_dcb');")
        check("Z3 sibling column's sequence survives",
              err is None and rows == [["1"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("alter table v98_dco drop column b cascade;")
        assert err is None, f"drop column b: {err}"
        rows, err, _ = c.q("select nextval('v98_dcb');")
        check("Z4 cascade drop of the column drops its owned sequence",
              err is not None and "42P01" in err, f"rows={rows} err={err}")

        # --- 12. OWNED BY owner restriction (PG19 42832) ---
        rows, err, _ = c.q("create role v98_owr;")
        assert err is None, f"create role: {err}"
        rows, err, _ = c.q("create table v98_owt(a int);")
        assert err is None
        rows, err, _ = c.q("create sequence v98_ows;")
        assert err is None
        rows, err, _ = c.q("alter sequence v98_ows owned by v98_owt.a;")
        check("O1 same-owner link accepted", err is None, f"err={err}")
        rows, err, _ = c.q("alter table v98_owt owner to v98_owr;")
        assert err is None, f"owner to: {err}"
        rows, err, _ = c.q("alter sequence v98_ows owned by none;")
        assert err is None
        rows, err, _ = c.q("alter sequence v98_ows owned by v98_owt.a;")
        check("O2 owner mismatch refused with 42832",
              err is not None and "42832" in err, f"err={err}")
        rows, err, _ = c.q("create sequence v98_ows2 owned by v98_owt.a;")
        check("O3 mismatched CREATE refused with 42832",
              err is not None and "42832" in err, f"err={err}")
        rows, err, _ = c.q(
            "select sequencename from pg_sequences where sequencename='v98_ows2';")
        check("O4 refused CREATE leaks no sequence",
              err is None and rows == [], f"rows={rows} err={err}")

        # --- 13. defaults, descending sequences, cycle, overflow ---
        rows, err, _ = c.q("create sequence v98_def;")
        assert err is None
        rows, err, _ = c.q(
            "select start_value, min_value, max_value, increment_by "
            "from pg_sequences where sequencename='v98_def';")
        check("D1 ascending defaults",
              rows == [["1", "1", "9223372036854775807", "1"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_neg increment by -1;")
        assert err is None
        rows, err, _ = c.q(
            "select start_value, min_value, max_value, increment_by "
            "from pg_sequences where sequencename='v98_neg';")
        check("D2 descending defaults (PG19: min -2^63)",
              rows == [["-1", "-9223372036854775808", "-1", "-1"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_neg'), nextval('v98_neg');")
        check("D3 descending nextval", rows == [["-1", "-2"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v98_ex start with 9 maxvalue 10;")
        assert err is None
        rows, err, _ = c.q("select nextval('v98_ex'), nextval('v98_ex');")
        check("D4 ascending to the top", rows == [["9", "10"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_ex');")
        check("D5 exhaustion is 22000", err == "22000", f"rows={rows} err={err}")
        rows, err, _ = c.q("alter sequence v98_ex restart;")
        assert err is None, f"restart: {err}"
        rows, err, _ = c.q("select nextval('v98_ex');")
        check("D6 bare RESTART resets to start", rows == [["9"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q(
            "create sequence v98_cy start with 9 minvalue 1 maxvalue 10 cycle;")
        assert err is None
        rows, err, _ = c.q(
            "select nextval('v98_cy'), nextval('v98_cy'), "
            "nextval('v98_cy'), nextval('v98_cy');")
        check("D7 cycle wraps to min_value", rows == [["9", "10", "1", "2"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q(
            "create sequence v98_dex start with 2 minvalue 1 maxvalue 10 "
            "increment by -1;")
        assert err is None
        rows, err, _ = c.q("select nextval('v98_dex'), nextval('v98_dex');")
        check("D8 descending to the bottom", rows == [["2", "1"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v98_dex');")
        check("D9 descending exhaustion is 22000", err == "22000",
              f"rows={rows} err={err}")
        rows, err, _ = c.q(
            "create sequence v98_dcy start with 1 minvalue 1 maxvalue 3 "
            "increment by -1 cycle;")
        assert err is None
        rows, err, _ = c.q(
            "select nextval('v98_dcy'), nextval('v98_dcy'), nextval('v98_dcy');")
        check("D10 descending cycle wraps to max_value",
              rows == [["1", "3", "2"]], f"rows={rows} err={err}")

        c.close()
        c2.close()
    finally:
        proc.terminate()
        proc.wait(timeout=15)

    print(f"v0.98 sequences: {PASS} passed, {FAIL} failed")
    sys.exit(0 if FAIL == 0 else 1)

if __name__ == "__main__":
    main()
