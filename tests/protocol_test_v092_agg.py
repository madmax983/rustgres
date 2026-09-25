#!/usr/bin/env python3
r"""v0.92 protocol tests: array_agg PG19 corrections + ORDER BY in aggregates.

Covers the v0.92 behavior changes (all grounded against PG19 source):
- `array_agg(x)` keeps scalar NULLs ("Collects all the input values,
  including nulls, into an array"); zero input rows -> NULL.
- `array_agg(a)` over array input: NULL input -> 22004 "cannot accumulate
  null arrays"; empty first input -> 2202E "cannot accumulate empty
  arrays"; later empty / mismatched dims / mismatched lower bounds ->
  2202E "cannot accumulate arrays of different dimensionality".
- `agg(x ORDER BY ...)`: PG19 docs section 4.2.7 — supported for grouped
  aggregates (array_agg, string_agg with delimiter alignment, sum, ...),
  including DESC and NULLS FIRST/LAST; DISTINCT requires the ORDER BY
  expressions to match arguments (42P10, PG19's exact message).
- Windowed `agg(x ORDER BY ...) OVER (...)` -> 0A000 "aggregate ORDER BY
  is not implemented for window functions" (PG19 parse_func.c verbatim).

Self-starting: launches rustgres on 5441 with PGDATESTYLE=Postgres, MDY.
"""
import socket, struct, subprocess, sys, time, os

PORT = 5441
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
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=15)
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

def main():
    global PASS, FAIL
    env = dict(os.environ, PGDATESTYLE="Postgres, MDY")
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", "/tmp/rg_proto_v092"],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(2)
    try:
        c = Conn()

        # --- array_agg NULL semantics (v0.92 correction) ---
        rows, err = c.q("select array_agg(i) from (values (1),(null),(3)) v(i);")
        check("N1 mixed nulls kept", rows == [["{1,NULL,3}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) from (values (null),(null)) v(i);")
        check("N2 all-null -> {NULL,NULL}", rows == [["{NULL,NULL}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) from (select 1 as i where false) t;")
        check("N3 empty -> NULL", rows == [[None]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(distinct i) from (values (1),(null),(1),(null)) v(i);")
        check("N4 distinct keeps one NULL", rows == [["{1,NULL}"]], f"rows={rows} err={err}")

        rows, err = c.q("select g, array_agg(i) from (values (1,1),(1,null),(2,3)) v(g,i) group by g order by g;")
        check("N5 grouped nulls kept", rows == [["1", "{1,NULL}"], ["2", "{3}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) over () from (values (1),(null),(3)) v(i);")
        check("N6 window nulls kept", rows == [["{1,NULL,3}"]] * 3, f"rows={rows} err={err}")

        # --- array_agg(anyarray) errors ---
        rows, err = c.q("select array_agg(a) from (values (array[1,2]),(null)) v(a);")
        check("R1 null array -> 22004", err == "22004", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(a) from (values (null),(array[1,2])) v(a);")
        check("R2 leading null array -> 22004", err == "22004", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(a) from (values ('{}'::int[]),(array[1,2])) v(a);")
        check("R3 empty first -> 2202E", err == "2202E", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(a) from (values (array[1,2]),('{}'::int[])) v(a);")
        check("R4 empty later -> 2202E", err == "2202E", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(a) from (values (array[1,2]),(array[1,2,3])) v(a);")
        check("R5 dim mismatch -> 2202E", err == "2202E", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(a) from (values (array[1,2]),(array[[1,2],[3,4]])) v(a);")
        check("R6 ndim mismatch -> 2202E", err == "2202E", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(a) from (values (array[1,2]),(array[1,2])) v(a);")
        check("R7 compatible arrays", rows == [["{{1,2},{1,2}}"]], f"rows={rows} err={err}")

        # --- ORDER BY inside grouped aggregates ---
        rows, err = c.q("select array_agg(x order by y) from (values (1,3),(2,1),(3,2)) v(x,y);")
        check("O1 array_agg order by", rows == [["{2,3,1}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(x order by y desc) from (values (1,3),(2,1),(3,2)) v(x,y);")
        check("O2 array_agg order by desc", rows == [["{1,3,2}"]], f"rows={rows} err={err}")

        rows, err = c.q("select string_agg(x, ',' order by y) from (values ('a',2),('b',1)) v(x,y);")
        check("O3 string_agg order by", rows == [["b,a"]], f"rows={rows} err={err}")

        rows, err = c.q("select string_agg(x, d order by y) from (values ('a','-',2),('b','+',1)) v(x,d,y);")
        check("O4 string_agg delimiter alignment", rows == [["b-a"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(x order by y nulls first) from (values (1,1),(2,null),(3,2)) v(x,y);")
        check("O5 nulls first", rows == [["{2,1,3}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(x order by y nulls last) from (values (1,1),(2,null),(3,2)) v(x,y);")
        check("O6 nulls last", rows == [["{1,3,2}"]], f"rows={rows} err={err}")

        rows, err = c.q("select g, array_agg(x order by y) from (values (1,'b',2),(1,'a',1),(2,'c',3)) v(g,x,y) group by g order by g;")
        check("O7 grouped order by", rows == [["1", "{a,b}"], ["2", "{c}"]], f"rows={rows} err={err}")

        rows, err = c.q("select sum(x order by y) from (values (1,2),(2,1)) v(x,y);")
        check("O8 sum order by accepted", rows == [["3"]], f"rows={rows} err={err}")

        rows, err = c.q("select count(*) from (values (1),(2)) v(x) order by 1;")
        check("O9 count star", rows == [["2"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(distinct x order by x) from (values (2),(1),(2)) v(x);")
        check("O10 distinct order by arg ok", rows == [["{1,2}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(distinct x order by x desc) from (values (2),(1),(2)) v(x);")
        check("O11 distinct order by arg desc", rows == [["{2,1}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(distinct x order by y) from (values (1,2),(2,1)) v(x,y);")
        check("O12 distinct order by non-arg -> 42P10", err == "42P10", f"rows={rows} err={err}")

        # PG deduplicates DISTINCT aggregate input ROWS: ('a','-') and
        # ('a','+') are distinct inputs, so both survive; ORDER BY x,d
        # pins the row order, and each value is preceded by its own
        # delimiter ('a' then '-','a').
        rows, err = c.q("select string_agg(distinct x, d order by x, d) from (values ('a','-'),('a','+')) v(x,d);")
        check("O13 distinct dedupes (value,delimiter) rows", rows == [["a-a"]], f"rows={rows} err={err}")

        rows, err = c.q("select string_agg(distinct x, ',') from (values ('a'),('a'),('b')) v(x);")
        check("O14 distinct const delimiter", rows == [["a,b"]], f"rows={rows} err={err}")

        # --- windowed aggregate ORDER BY / DISTINCT rejected like PG19 ---
        rows, err = c.q("select array_agg(x order by y) over () from (values (1,2)) v(x,y);")
        check("W1 window order by -> 0A000", err == "0A000", f"rows={rows} err={err}")

        rows, err = c.q("select sum(distinct x) over () from (values (1),(1)) v(x);")
        check("W2 window distinct -> 0A000", err == "0A000", f"rows={rows} err={err}")

        rows, err = c.q("select sum(x) over () from (values (1),(2)) v(x);")
        check("W3 plain windowed agg ok", rows == [["3"], ["3"]], f"rows={rows} err={err}")

        # --- static array_agg overload discrimination (v0.92 fix) ---
        # PG picks array_agg(anyarray) by STATIC type: an all-NULL
        # array-typed argument raises 22004, not scalar {NULL,...}.
        rows, err = c.q("select array_agg(null::int[]) from (values (1),(2)) v(x);")
        check("S1 static array all-NULL -> 22004", err == "22004", f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(null::int[]) over () from (values (1),(2)) v(x);")
        check("S2 windowed static array all-NULL -> 22004", err == "22004", f"rows={rows} err={err}")

        # Scalar all-NULL still keeps NULLs (not 22004).
        rows, err = c.q("select array_agg(null::int) from (values (1),(2)) v(x);")
        check("S3 static scalar all-NULL kept", rows == [["{NULL,NULL}"]], f"rows={rows} err={err}")

        c.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)

    print(f"\n{PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0

if __name__ == "__main__":
    sys.exit(main())
