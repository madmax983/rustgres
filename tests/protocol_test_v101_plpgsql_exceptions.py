#!/usr/bin/env python3
r"""v1.01 protocol tests: bounded PL/pgSQL statement sequences + EXCEPTION.

Covers the v1.01 plpgsql extension, grounded in PostgreSQL REL_19_STABLE
(src/pl/plpgsql/src/pl_gram.y, pl_comp.c, pl_exec.c):

- The exact transactions.sql REAL-FAIL shape: inverse() with ANALYZE in
  the body and WHEN division_by_zero trapping 22012.
- Statement sequences (utility + RETURN) run in order.
- EXCEPTION/WHEN: exact SQLSTATE match, category (22000) match, OTHERS,
  WHEN SQLSTATE '...', first-match-wins, handler errors propagate,
  untrapped errors propagate with their own code.
- Unsupported bodies stay honest 0A000; unknown condition names are
  42704; malformed RETURN is 42601.
- Restart durability: the parsed plpgsql body is rebuilt from stored
  source on WAL replay / checkpoint restore (FuncDef.plpgsql).

Self-starting: launches rustgres on 5447 with a fresh datadir, issues
CHECKPOINT and restarts mid-run to exercise WAL replay and checkpoint
recovery.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5447
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
DATADIR = "/tmp/rg_proto_v101_plpgsql"

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

        # --- The transactions.sql REAL-FAIL shape, verbatim ---
        r, e = c.q("create table revalidate_bug (c float8 unique);")
        check("inverse: create table", e is None, e)
        r, e = c.q("""create function inverse(int) returns float8 as
$$
begin
  analyze revalidate_bug;
  return 1::float8/$1;
exception
  when division_by_zero then return 0;
end$$ language plpgsql volatile;""")
        check("inverse: create function", e is None, e)
        r, e = c.q("insert into revalidate_bug values (inverse(0));")
        check("inverse: insert inverse(0) traps 22012", e is None, e)
        r, e = c.q("select c from revalidate_bug order by c;")
        check("inverse: trapped row is 0", r == [['0']], f"{r} {e}")
        r, e = c.q("select inverse(2);")
        check("inverse: inverse(2) = 0.5", r == [['0.5']], f"{r} {e}")
        r, e = c.q("select inverse(0);")
        check("inverse: inverse(0) = 0", r == [['0']], f"{r} {e}")

        # --- Statement sequence: utility runs, then RETURN ---
        r, e = c.q("""create function seq101() returns int as
'begin analyze revalidate_bug; return 42; end' language plpgsql volatile;""")
        check("seq: create", e is None, e)
        r, e = c.q("select seq101();")
        check("seq: returns 42", r == [['42']], f"{r} {e}")

        # --- Untrapped error propagates with its own code ---
        r, e = c.q("""create function untrapped101() returns int as
'begin return 1/0; exception when unique_violation then return 9; end'
language plpgsql volatile;""")
        check("untrapped: create", e is None, e)
        r, e = c.q("select untrapped101();")
        check("untrapped: 22012 propagates", e == "22012", e)

        # --- Category condition, OTHERS, SQLSTATE literal ---
        r, e = c.q("""create function cat101() returns int as
'begin return 1/0; exception when data_exception then return 7; end'
language plpgsql volatile;""")
        check("category: create", e is None, e)
        r, e = c.q("select cat101();")
        check("category: 22xxx traps 22012", r == [['7']], f"{r} {e}")
        r, e = c.q("""create function oth101() returns int as
'begin return 1/0; exception when others then return 9; end'
language plpgsql volatile;""")
        check("others: create", e is None, e)
        r, e = c.q("select oth101();")
        check("others: traps 22012", r == [['9']], f"{r} {e}")
        r, e = c.q("""create function sq101() returns int as
$$begin return 1/0; exception when sqlstate '22012' then return 5; end$$
language plpgsql volatile;""")
        check("sqlstate: create", e is None, e)
        r, e = c.q("select sq101();")
        check("sqlstate: literal traps 22012", r == [['5']], f"{r} {e}")

        # --- First matching handler wins ---
        r, e = c.q("""create function first101() returns int as
'begin return 1/0; exception when division_by_zero then return 1; when others then return 2; end'
language plpgsql volatile;""")
        check("first-match: create", e is None, e)
        r, e = c.q("select first101();")
        check("first-match: wins", r == [['1']], f"{r} {e}")

        # --- Handler errors propagate untrapped ---
        r, e = c.q("""create function herr101() returns int as
'begin return 1/0; exception when division_by_zero then return 1/0; end'
language plpgsql volatile;""")
        check("handler-err: create", e is None, e)
        r, e = c.q("select herr101();")
        check("handler-err: 22012 propagates", e == "22012", e)

        # --- Named argument references rewrite to $n ---
        r, e = c.q("""create function named101(x int) returns int as
'begin return x + 1; exception when others then return 0; end'
language plpgsql volatile;""")
        check("named-arg: create", e is None, e)
        r, e = c.q("select named101(41);")
        check("named-arg: x rewrites to $1", r == [['42']], f"{r} {e}")

        # --- Honest errors: 0A000 / 42704 / 42601 ---
        r, e = c.q("""create function bad101a() returns int as
'begin x := 1; return 1; end' language plpgsql;""")
        # v1.03: `:=` is supported but requires DECLARE — undeclared
        # target is 42601 (was 0A000 in v1.01).
        check("42601: assignment needs DECLARE", e == "42601", e)
        r, e = c.q("""create function bad101b() returns int as
'begin insert into revalidate_bug values (1); return 1; end' language plpgsql;""")
        check("0A000: insert unsupported", e == "0A000", e)
        r, e = c.q("""create function bad101c() returns int as
'begin return 1; exception when nosuchcond then return 0; end' language plpgsql;""")
        check("42704: unknown condition", e == "42704", e)
        r, e = c.q("""create function bad101d() returns int as
'begin return; end' language plpgsql;""")
        check("42601: empty RETURN", e == "42601", e)

        # --- Restart durability: WAL replay + checkpoint restore ---
        c.q("CHECKPOINT;")
        c.close()
        stop_server(proc)
        proc = start_server()
        c = Conn()
        r, e = c.q("select inverse(0);")
        check("restart: inverse(0) still traps", r == [['0']], f"{r} {e}")
        r, e = c.q("select inverse(4);")
        check("restart: inverse(4) = 0.25", r == [['0.25']], f"{r} {e}")
        r, e = c.q("select named101(41);")
        check("restart: named-arg rewrite survives", r == [['42']], f"{r} {e}")
        r, e = c.q("select c from revalidate_bug order by c;")
        check("restart: table rows survive", r == [['0']], f"{r} {e}")
        r, e = c.q("insert into revalidate_bug values (inverse(2));")
        check("restart: insert via function works", e is None, e)
        r, e = c.q("select c from revalidate_bug order by c;")
        check("restart: both rows present", r == [['0'], ['0.5']], f"{r} {e}")

        c.close()
    finally:
        stop_server(proc)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)

if __name__ == "__main__":
    main()
