#!/usr/bin/env python3
r"""v1.03 protocol tests: scalar DECLARE/:=, FOR..LOOP, RETURN NEXT, RETURNS SETOF, EXPLAIN (ANALYZE).

Covers the v1.03 plpgsql extension, grounded in PostgreSQL REL_19_STABLE
(pl_gram.y declaration/assignment grammar, pl_exec.c `exec_stmt_fors`,
`exec_stmt_return_next`):

- Scalar DECLARE + `:=` + RETURN: variables bind to local slots,
  references rewrite to positional params.
- FOR var IN <query> LOOP: variable binds per row; RETURN NEXT
  accumulates SETOF rows.
- The exact subselect.sql REAL-FAIL shape: explain_sq_limit() with
  FOR over EXPLAIN (ANALYZE, ...) — 6 rows, exact plan text.
- Top-level EXPLAIN (ANALYZE) renders actual rows; planning-only
  EXPLAIN does not.
- Validation: RETURN NEXT in scalar (42601), RETURN in SETOF (42601),
  undeclared assignment target (42601), duplicate declaration (42601).
- Restart durability: CHECKPOINT + restart; the parsed plpgsql body
  (with DECLARE/FOR) is rebuilt from stored source on WAL replay /
  checkpoint restore.

Self-starting: launches rustgres on 5449 with a fresh datadir, issues
CHECKPOINT and restarts mid-run to exercise WAL replay and checkpoint
recovery.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5449
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
DATADIR = "/tmp/rg_proto_v103_loops"

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
        """Returns (rows, err_code). rows is list of lists."""
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
                    e = p.index(b"\x00", i + 1)
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

        # --- Scalar DECLARE + := + RETURN ---
        r, e = c.q("""CREATE FUNCTION add103(a int, b int) RETURNS int AS $$
declare s int;
begin
  s := a + b;
  return s;
end;
$$ LANGUAGE plpgsql""")
        check("scalar declare/assign: create", e is None, e)
        r, e = c.q("SELECT add103(3, 4)")
        check("scalar declare/assign: call", e is None and r == [["7"]], f"{e} {r}")

        # Variable references in expressions.
        r, e = c.q("""CREATE FUNCTION dbl103(x int) RETURNS int AS $$
declare y int;
begin
  y := x * 2;
  return y + x;
end;
$$ LANGUAGE plpgsql""")
        check("scalar var refs: create", e is None, e)
        r, e = c.q("SELECT dbl103(5)")
        check("scalar var refs: call", e is None and r == [["15"]], f"{e} {r}")

        # --- FOR over SELECT + RETURN NEXT (SETOF) ---
        r, e = c.q("""CREATE FUNCTION gen103(n int) RETURNS SETOF int AS $$
declare i int;
begin
  for i in select * from generate_series(1, n) loop
    return next i * 10;
  end loop;
end;
$$ LANGUAGE plpgsql""")
        check("setof for/select: create", e is None, e)
        r, e = c.q("SELECT * FROM gen103(3)")
        check("setof for/select: call", e is None and r == [["10"], ["20"], ["30"]], f"{e} {r}")

        # --- The subselect.sql REAL-FAIL shape, verbatim ---
        r, e = c.q("CREATE TABLE sq_limit (pk int primary key, c1 int, c2 int)")
        check("sq_limit: create table", e is None, e)
        r, e = c.q("""INSERT INTO sq_limit VALUES
(1, 1, 1), (2, 2, 2), (3, 3, 3), (4, 4, 4),
(5, 1, 1), (6, 2, 2), (7, 3, 3), (8, 4, 4)""")
        check("sq_limit: insert", e is None, e)
        r, e = c.q("""CREATE FUNCTION explain_sq_limit() RETURNS SETOF text LANGUAGE plpgsql AS $$
declare ln text;
begin
    for ln in
        explain (analyze, summary off, timing off, costs off, buffers off)
        select * from (select pk,c2 from sq_limit order by c1,pk) as x limit 3
    loop
        ln := regexp_replace(ln, 'Memory: \\S*',  'Memory: xxx');
        return next ln;
    end loop;
end;
$$""")
        check("explain_sq_limit: create", e is None, e)
        r, e = c.q("SELECT * FROM explain_sq_limit()")
        expected = [
            ["Limit (actual rows=3.00 loops=1)"],
            ["   ->  Subquery Scan on x (actual rows=3.00 loops=1)"],
            ["         ->  Sort (actual rows=3.00 loops=1)"],
            ["               Sort Key: sq_limit.c1, sq_limit.pk"],
            ["               Sort Method: top-N heapsort  Memory: xxx"],
            ["               ->  Seq Scan on sq_limit (actual rows=8.00 loops=1)"],
        ]
        check("explain_sq_limit: exact plan", e is None and r == expected, f"{e} {r}")

        # --- Top-level EXPLAIN (ANALYZE) ---
        r, e = c.q("EXPLAIN (ANALYZE) SELECT * FROM sq_limit")
        check("explain analyze: toplevel", e is None and r and "actual rows=8.00" in r[0][0], f"{e} {r}")
        r, e = c.q("EXPLAIN SELECT * FROM sq_limit")
        check("explain planning-only: no actual", e is None and r and "actual" not in r[0][0], f"{e} {r}")

        # --- Validation errors ---
        r, e = c.q("CREATE FUNCTION rn103() RETURNS int AS $$ begin return next 1; end; $$ LANGUAGE plpgsql")
        check("validation: return next in scalar", e == "42601", e)
        r, e = c.q("CREATE FUNCTION r103() RETURNS SETOF int AS $$ begin return 1; end; $$ LANGUAGE plpgsql")
        check("validation: return in setof", e == "42601", e)
        r, e = c.q("CREATE FUNCTION u103() RETURNS int AS $$ declare x int; begin y := 1; return x; end; $$ LANGUAGE plpgsql")
        check("validation: undeclared assign", e == "42601", e)
        r, e = c.q("CREATE FUNCTION d103() RETURNS int AS $$ declare x int; x text; begin return 1; end; $$ LANGUAGE plpgsql")
        check("validation: duplicate decl", e == "42601", e)

        # --- Restart durability: CHECKPOINT + restart ---
        r, e = c.q("CHECKPOINT")
        check("checkpoint", e is None, e)
        c.close()
        stop_server(proc)
        proc = start_server()
        c = Conn()
        r, e = c.q("SELECT add103(10, 20)")
        check("restart: scalar function", e is None and r == [["30"]], f"{e} {r}")
        r, e = c.q("SELECT * FROM gen103(2)")
        check("restart: setof function", e is None and r == [["10"], ["20"]], f"{e} {r}")
        r, e = c.q("SELECT * FROM explain_sq_limit()")
        check("restart: explain_sq_limit", e is None and r == expected, f"{e} {r}")

        c.close()
    finally:
        stop_server(proc)

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)

if __name__ == "__main__":
    main()
