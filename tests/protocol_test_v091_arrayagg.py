#!/usr/bin/env python3
r"""v0.91 protocol tests: array_agg aggregate + op ANY/ALL/SOME (array_expr).

Covers the v0.91 behavior changes:
- `array_agg(x)`: collects non-null inputs in row order into a 1-D array
  (`{1,2,3}` text form); NULL inputs skipped; zero non-null inputs -> NULL;
  `DISTINCT` dedupes; `pg_typeof` -> `integer[]` etc.; multidimensional
  when the input is an array (`{{1,2},{2,3}}`); works as a window function.
- `expr op ANY|ALL|SOME (array_expr)`: PG19 ScalarArrayOp with
  three-valued logic (NULL array -> NULL, `{}` -> false/true for ANY/ALL,
  NULL elements participate in 3VL, non-array right side -> 42821),
  row-wise `(a,b) = ANY(array[...])` included.
- The B7 text-suite case: format(string_agg('%s',','), variadic
  array_agg(i)) over generate_series(1,200) -> '1,2,...,200'.

Self-starting: launches rustgres on 5440 with PGDATESTYLE=Postgres, MDY.
"""
import socket, struct, subprocess, sys, time, os

PORT = 5440
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
        [BIN, "--port", str(PORT), "--data-dir", "/tmp/rg_proto_v091"],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(2)
    try:
        c = Conn()

        # --- array_agg ---
        rows, err = c.q("select array_agg(i) from generate_series(1,5) g(i);")
        check("A1 basic", rows == [["{1,2,3,4,5}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) from (values (1),(null),(3)) v(i);")
        check("A2 nulls skipped", rows == [["{1,3}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) from (select 1 as i where false) t;")
        check("A3 empty -> NULL", rows == [[None]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) from (values (null),(null)) v(i);")
        check("A3b all-null -> NULL", rows == [[None]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(distinct i) from (values (1),(2),(1)) v(i);")
        check("A4 distinct", rows == [["{1,2}"]], f"rows={rows} err={err}")

        rows, err = c.q("select pg_typeof(array_agg(i)) from generate_series(1,3) g(i);")
        check("A5 typeof integer[]", rows == [["integer[]"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(x) from (values ('a'),('b')) v(x);")
        check("A6 text", rows == [["{a,b}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(array[i, i+1]) from generate_series(1,2) g(i);")
        check("A7 multidim", rows == [["{{1,2},{2,3}}"]], f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(i) over () from generate_series(1,3) g(i);")
        check("A8 window", rows == [["{1,2,3}"]] * 3, f"rows={rows} err={err}")

        rows, err = c.q("select array_agg(1.5::numeric);")
        check("A9 numeric", rows == [["{1.5}"]], f"rows={rows} err={err}")

        rows, err = c.q(
            "select format(string_agg('%s',','), variadic array_agg(i)) "
            "from generate_series(1,200) g(i);"
        )
        want = ",".join(str(i) for i in range(1, 201))
        check("A10 B7 200-elem variadic", rows == [[want]], f"rows={str(rows)[:80]} err={err}")

        # --- op ANY/ALL/SOME (array_expr) ---
        rows, err = c.q("select 1 = any(array[1,2]);")
        check("Q1 any true", rows == [["t"]], f"rows={rows} err={err}")

        rows, err = c.q("select 5 = any(array[1,2]);")
        check("Q2 any false", rows == [["f"]], f"rows={rows} err={err}")

        rows, err = c.q("select 5 > all(array[1,2]);")
        check("Q3 all true", rows == [["t"]], f"rows={rows} err={err}")

        rows, err = c.q("select 1 > all(array[1,2]);")
        check("Q4 all false", rows == [["f"]], f"rows={rows} err={err}")

        rows, err = c.q("select null = any(array[1,2]);")
        check("Q5 null left -> NULL", rows == [[None]], f"rows={rows} err={err}")

        rows, err = c.q("select 5 = any(array[1,null]);")
        check("Q6 null elem, no match -> NULL", rows == [[None]], f"rows={rows} err={err}")

        rows, err = c.q("select 1 = any(array[1,null]);")
        check("Q7 null elem, match -> t", rows == [["t"]], f"rows={rows} err={err}")

        rows, err = c.q("select 1 = any('{}'::int[]);")
        check("Q8 empty any -> f", rows == [["f"]], f"rows={rows} err={err}")

        rows, err = c.q("select 1 = all('{}'::int[]);")
        check("Q9 empty all -> t", rows == [["t"]], f"rows={rows} err={err}")

        rows, err = c.q("select 1 = any(null::int[]);")
        check("Q10 null array -> NULL", rows == [[None]], f"rows={rows} err={err}")

        rows, err = c.q("select 1 = any(5);")
        check("Q11 non-array -> 42821", err == "42821", f"err={err}")

        rows, err = c.q("select (1,2) = any(array[(1,2),(3,4)]);")
        check("Q12 row-wise true", rows == [["t"]], f"rows={rows} err={err}")

        rows, err = c.q("select (1,2) = any(array[(1,3)]);")
        check("Q13 row-wise false", rows == [["f"]], f"rows={rows} err={err}")

        rows, err = c.q("select 2 = some(array[1,2]);")
        check("Q14 some", rows == [["t"]], f"rows={rows} err={err}")

        rows, err = c.q("select (1 = any(array_agg(i))) from generate_series(1,3) g(i);")
        check("Q15 array_agg consumer", rows == [["t"]], f"rows={rows} err={err}")

        # v0.91: non-array VARIADIC argument -> 42821 (PG19 "VARIADIC argument
        # must be an array"); array and NULL-array forms keep working.
        rows, err = c.q("select concat_ws(',', variadic 10);")
        check("Q16 variadic non-array -> 42821", err == "42821", f"rows={rows} err={err}")

        rows, err = c.q("select concat(variadic array[1,2,3]);")
        check("Q17 variadic array ok", rows == [["123"]], f"rows={rows} err={err}")

        rows, err = c.q("select concat_ws(',', variadic NULL::int[]);")
        check("Q18 variadic null array -> NULL", rows == [[None]], f"rows={rows} err={err}")

        c.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)

    print(f"\n{PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0

if __name__ == "__main__":
    sys.exit(main())
