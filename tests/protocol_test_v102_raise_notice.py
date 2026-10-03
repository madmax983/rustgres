#!/usr/bin/env python3
r"""v1.02 protocol tests: bounded PL/pgSQL RAISE NOTICE / RAISE EXCEPTION.

Covers the v1.02 plpgsql extension, grounded in PostgreSQL REL_19_STABLE
(src/pl/plpgsql/src/pl_exec.c `exec_stmt_raise`, pl_gram.y
`check_raise_parameters`):

- The exact subselect.sql REAL-FAIL shape: tattle() with
  `raise notice 'x = %, y = %', x, y;` — function executes, boolean
  results are correct, and each call delivers a NoticeResponse ('N')
  with the rendered message before CommandComplete.
- %-format rendering: `%%` escape, multiple args, named-arg rewrite.
- RAISE EXCEPTION: aborts the call with P0001 and the rendered
  message; trappable by WHEN handlers.
- Unsupported levels (DEBUG/LOG/INFO/WARNING) are honest 0A000.
- Restart durability: CHECKPOINT + restart; the parsed plpgsql body
  (with RAISE) is rebuilt from stored source on WAL replay /
  checkpoint restore, notices still delivered.

Self-starting: launches rustgres on 5448 with a fresh datadir, issues
CHECKPOINT and restarts mid-run to exercise WAL replay and checkpoint
recovery.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5448
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
DATADIR = "/tmp/rg_proto_v102_raise"

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

def notice_message(payload):
    """Extract the M (message) field from a NoticeResponse payload."""
    i = 0
    fields = {}
    while i < len(payload) - 1:
        f = payload[i:i+1]
        e = payload.index(b"\x00", i + 1)
        fields[f] = payload[i+1:e].decode()
        i = e + 1
    return fields.get(b"M", "")

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
        """Returns (rows, err_code, notices). rows is list of lists."""
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
            elif t == b"N":
                notices.append(notice_message(p))
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
    shutil.rmtree(DATADIR, ignore_errors=True)
    proc = start_server()
    try:
        c = Conn()

        # --- The subselect.sql REAL-FAIL shape, verbatim ---
        r, e, n = c.q("""create function tattle(x int, y int) returns bool
volatile language plpgsql as $$
begin
  raise notice 'x = %, y = %', x, y;
  return x > y;
end$$;""")
        check("tattle: create function", e is None, e)
        r, e, n = c.q("select tattle(9, 8);")
        check("tattle: tattle(9,8) is true", r == [['t']], f"{r} {e}")
        check("tattle: notice delivered", n == ["x = 9, y = 8"], f"{n}")
        r, e, n = c.q("select tattle(1, 8);")
        check("tattle: tattle(1,8) is false", r == [['f']], f"{r} {e}")
        check("tattle: notice delivered (2)", n == ["x = 1, y = 8"], f"{n}")

        # --- tattle in a WHERE clause over rows (the conformance shape) ---
        r, e, n = c.q("create table ten102(x int);")
        check("where: create table", e is None, e)
        r, e, n = c.q("insert into ten102 values (9), (1), (8);")
        check("where: insert", e is None, e)
        r, e, n = c.q("select x from ten102 where tattle(x, 8) order by 1;")
        check("where: tattle(x,8) filters", r == [['9']], f"{r} {e}")
        check("where: one notice per call", n == ["x = 9, y = 8", "x = 1, y = 8", "x = 8, y = 8"], f"{n}")

        # --- Format edge cases over the wire ---
        r, e, n = c.q("""create function fmt102(a int, b int) returns int
volatile language plpgsql as $$
begin
  raise notice '100%% of %', a;
  raise notice 'pair: %, %', a, b;
  return a;
end$$;""")
        check("format: create", e is None, e)
        r, e, n = c.q("select fmt102(3, 4);")
        check("format: returns arg", r == [['3']], f"{r} {e}")
        check("format: %% escape + two args", n == ["100% of 3", "pair: 3, 4"], f"{n}")

        # --- RAISE EXCEPTION propagates with P0001 ---
        r, e, n = c.q("""create function boom102() returns int
volatile language plpgsql as $$
begin
  raise exception 'kaput %', 42;
  return 1;
end$$;""")
        check("exception: create", e is None, e)
        r, e, n = c.q("select boom102();")
        check("exception: P0001 propagates", e == "P0001", e)
        check("exception: no notice on the error path", n == [], f"{n}")

        # --- RAISE EXCEPTION is trappable by WHEN ---
        r, e, n = c.q("""create function trap102() returns int
volatile language plpgsql as $$
begin
  raise notice 'about to blow';
  raise exception 'nope';
  return 1;
exception when raise_exception then
  raise notice 'caught it';
  return -1;
end$$;""")
        check("trap: create", e is None, e)
        r, e, n = c.q("select trap102();")
        check("trap: handler ran", r == [['-1']], f"{r} {e}")
        check("trap: notices from body and handler", n == ["about to blow", "caught it"], f"{n}")

        # --- Unsupported levels stay honest 0A000 ---
        for level in ["debug", "log", "info", "warning"]:
            r, e, n = c.q(
                f"create function lvl102_{level}() returns int as "
                f"'begin raise {level} ''x''; return 1; end' language plpgsql;"
            )
            check(f"0A000: raise {level} unsupported", e == "0A000", e)

        # --- CHECKPOINT + restart durability ---
        c.q("CHECKPOINT;")
        c.close()
        stop_server(proc)
        proc = start_server()
        c = Conn()
        r, e, n = c.q("select tattle(9, 8);")
        check("restart: tattle(9,8) still true", r == [['t']], f"{r} {e}")
        check("restart: notice still delivered", n == ["x = 9, y = 8"], f"{n}")
        r, e, n = c.q("select x from ten102 where tattle(x, 8) order by 1;")
        check("restart: where-clause tattle works", r == [['9']], f"{r} {e}")
        check("restart: notices after replay", n == ["x = 9, y = 8", "x = 1, y = 8", "x = 8, y = 8"], f"{n}")
        r, e, n = c.q("select boom102();")
        check("restart: exception still P0001", e == "P0001", e)
        r, e, n = c.q("select trap102();")
        check("restart: handler still traps", r == [['-1']], f"{r} {e}")
        check("restart: handler notices survive", n == ["about to blow", "caught it"], f"{n}")

        c.close()
    finally:
        stop_server(proc)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)

if __name__ == "__main__":
    main()
