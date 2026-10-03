#!/usr/bin/env python3
r"""v0.99 protocol tests: sequence type, pg_sequences.cycle, restart durability.

Covers the v0.99 sequence changes, grounded in PostgreSQL REL_19_STABLE
(src/backend/commands/sequence.c, init_params):

- pg_sequences exposes `cycle` (bool), not `cycle_option` (that name is
  information_schema.sequences only).
- CREATE SEQUENCE ... AS smallint | int | bigint: explicit type stored;
  defaults are type-driven (ascending: min 1, max type-max, start min;
  descending: max -1, min type-min, start max); explicit bounds outside
  the type range are 22023.
- ALTER SEQUENCE ... AS: bounds reset to the new type's bounds only when
  the old bounds were the old type's defaults.
- Durability: CACHE, OWNED BY, and the sequence type survive both WAL
  replay and checkpoint/restart.

Self-starting: launches rustgres on 5445 with a fresh datadir, restarts
it mid-run to exercise WAL replay and checkpoint recovery.
"""
import socket, struct, subprocess, sys, time, os, signal

PORT = 5445
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
DATADIR = "/tmp/rg_proto_v099"

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
    global PASS, FAIL
    os.system(f"rm -rf {DATADIR}")
    proc = start_server()
    try:
        c = Conn()

        # --- 1. pg_sequences.cycle rename ---
        rows, err, _ = c.q("create sequence v99_cyc cycle cache 7;")
        assert err is None, f"setup v99_cyc: {err}"
        rows, err, _ = c.q(
            "select cycle, cache_size from pg_sequences where sequencename='v99_cyc';")
        check("P1 pg_sequences.cycle is bool true",
              err is None and rows == [["t", "7"]], f"rows={rows} err={err}")
        rows, err, _ = c.q("select cycle_option from pg_sequences;")
        check("P2 pg_sequences.cycle_option is gone",
              err == "42703", f"rows={rows} err={err}")
        rows, err, _ = c.q(
            "select cycle_option from information_schema.sequences "
            "where sequence_name='v99_cyc';")
        check("P3 information_schema keeps cycle_option",
              err is None and rows == [["YES"]], f"rows={rows} err={err}")

        # --- 2. explicit AS types ---
        rows, err, _ = c.q("create sequence v99_small as smallint;")
        assert err is None, f"v99_small: {err}"
        rows, err, _ = c.q("create sequence v99_int as integer;")
        assert err is None, f"v99_int: {err}"
        rows, err, _ = c.q("create sequence v99_big as bigint;")
        assert err is None, f"v99_big: {err}"
        rows, err, _ = c.q(
            "select sequencename, data_type, min_value, max_value, start_value "
            "from pg_sequences where sequencename like 'v99_%' order by 1;")
        # v99_big, v99_cyc, v99_int, v99_small (alphabetical)
        exp = [
            ["v99_big", "bigint", "1", "9223372036854775807", "1"],
            ["v99_cyc", "bigint", "1", "9223372036854775807", "1"],
            ["v99_int", "integer", "1", "2147483647", "1"],
            ["v99_small", "smallint", "1", "32767", "1"],
        ]
        check("P4 AS types with PG19 defaults",
              err is None and rows == exp, f"rows={rows} err={err}")
        # descending smallint: max -1, min type-min, start max
        rows, err, _ = c.q("create sequence v99_desc as smallint increment by -1;")
        assert err is None, f"v99_desc: {err}"
        rows, err, _ = c.q(
            "select min_value, max_value, start_value from pg_sequences "
            "where sequencename='v99_desc';")
        check("P5 descending smallint defaults",
              err is None and rows == [["-32768", "-1", "-1"]],
              f"rows={rows} err={err}")
        # out-of-range bounds -> 22023
        rows, err, _ = c.q("create sequence v99_bad1 as smallint maxvalue 100000;")
        check("P6 smallint maxvalue out of range -> 22023",
              err == "22023", f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v99_bad2 as integer minvalue -9999999999;")
        check("P7 integer minvalue out of range -> 22023",
              err == "22023", f"rows={rows} err={err}")
        # serial sequences are typed
        rows, err, _ = c.q("create table v99_sert (a smallserial, b serial, c bigserial);")
        assert err is None, f"v99_sert: {err}"
        rows, err, _ = c.q(
            "select sequencename, data_type from pg_sequences "
            "where sequencename like 'v99_sert%' order by 1;")
        check("P8 serial backing sequences are typed",
              err is None and rows == [
                  ["v99_sert_a_seq", "smallint"],
                  ["v99_sert_b_seq", "integer"],
                  ["v99_sert_c_seq", "bigint"],
              ], f"rows={rows} err={err}")

        # --- 3. ALTER ... AS ---
        rows, err, _ = c.q("alter sequence v99_big as smallint;")
        assert err is None, f"alter v99_big: {err}"
        rows, err, _ = c.q(
            "select data_type, min_value, max_value from pg_sequences "
            "where sequencename='v99_big';")
        check("P9 ALTER AS resets default bounds",
              err is None and rows == [["smallint", "1", "32767"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("create sequence v99_keep maxvalue 1000;")
        assert err is None, f"v99_keep: {err}"
        rows, err, _ = c.q("alter sequence v99_keep as smallint;")
        assert err is None, f"alter v99_keep: {err}"
        rows, err, _ = c.q(
            "select data_type, min_value, max_value from pg_sequences "
            "where sequencename='v99_keep';")
        check("P10 ALTER AS keeps explicit bounds",
              err is None and rows == [["smallint", "1", "1000"]],
              f"rows={rows} err={err}")

        # --- 4. durability: CACHE + OWNED BY + type across restarts ---
        rows, err, _ = c.q("create table v99_own (id serial primary key, v text);")
        assert err is None, f"v99_own: {err}"
        rows, err, _ = c.q(
            "create sequence v99_dur as smallint cache 25 owned by v99_own.v;")
        assert err is None, f"v99_dur: {err}"
        rows, err, _ = c.q("select nextval('v99_dur');")
        assert err is None and rows == [["1"]], f"nextval: {rows} {err}"
        c.close()

        # restart 1: WAL replay (no checkpoint)
        stop_server(proc)
        proc = start_server()
        c = Conn()
        rows, err, _ = c.q(
            "select data_type, cache_size, start_value from pg_sequences "
            "where sequencename='v99_dur';")
        check("P11 WAL replay keeps type+cache",
              err is None and rows == [["smallint", "25", "1"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v99_dur');")
        check("P12 WAL replay keeps position",
              err is None and rows == [["2"]], f"rows={rows} err={err}")
        # owned-by link survives: dropping the table drops the sequence
        rows, err, _ = c.q("drop table v99_own;")
        assert err is None, f"drop v99_own: {err}"
        rows, err, _ = c.q("select nextval('v99_dur');")
        check("P13 WAL replay keeps OWNED BY link",
              err == "42P01", f"rows={rows} err={err}")

        # restart 2: checkpoint/restart
        rows, err, _ = c.q("create sequence v99_ckpt as integer cache 13;")
        assert err is None, f"v99_ckpt: {err}"
        rows, err, _ = c.q("select nextval('v99_ckpt');")
        assert err is None, f"nextval ckpt: {err}"
        rows, err, _ = c.q("checkpoint;")
        assert err is None, f"checkpoint: {err}"
        c.close()
        stop_server(proc)
        proc = start_server()
        c = Conn()
        rows, err, _ = c.q(
            "select data_type, cache_size from pg_sequences "
            "where sequencename='v99_ckpt';")
        check("P14 checkpoint keeps type+cache",
              err is None and rows == [["integer", "13"]],
              f"rows={rows} err={err}")
        rows, err, _ = c.q("select nextval('v99_ckpt');")
        check("P15 checkpoint keeps position",
              err is None and rows == [["2"]], f"rows={rows} err={err}")
        # pg_sequences.cycle still correct after restart
        rows, err, _ = c.q(
            "select cycle from pg_sequences where sequencename='v99_cyc';")
        check("P16 cycle survives restart",
              err is None and rows == [["t"]], f"rows={rows} err={err}")
        c.close()
    finally:
        stop_server(proc)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)

if __name__ == "__main__":
    main()
