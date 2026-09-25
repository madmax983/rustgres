#!/usr/bin/env python3
r"""v0.90 protocol tests: text operator and concat/variadic fixes (Cluster B).

Covers the v0.90 behavior changes:
- `||` operator: `3 || 4.0` (integer || numeric) -> 42883 (PG19 text.out);
  text || int still works via implicit cast
- concat/concat_ws with date values render MM-DD-YYYY under
  PGDATESTYLE=Postgres, MDY (pg_regress sets this; PG19 text.out)
- `VARIADIC` keyword: `concat(variadic NULL::int[])` is NULL,
  `concat(variadic '{}'::int[])` is '', `concat_ws` with variadic NULL is NULL
- bool values render as 't'/'f' in concat (already correct, regression-guarded)

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
                # Extract SQLSTATE (field 'C')
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
    # Start server with PGDATESTYLE=Postgres, MDY (like pg_regress)
    env = dict(os.environ, PGDATESTYLE="Postgres, MDY")
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", "/tmp/rg_proto_v090"],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(2)
    try:
        c = Conn()

        # B1: 3 || 4.0 -> 42883
        rows, err = c.q("select 3 || 4.0;")
        check("B1: 3 || 4.0 -> 42883", err == "42883", f"err={err}")

        # B1 control: text || int works
        rows, err = c.q("select 'four: ' || 2+2;")
        check("B1c: 'four: ' || 2+2", rows == [["four: 4"]], f"rows={rows} err={err}")

        # B1 control: unknown || int works
        rows, err = c.q("select 'four: ' || 2;")
        check("B1c2: 'four: ' || 2", rows == [["four: 2"]], f"rows={rows} err={err}")

        # B2: concat with date -> Postgres datestyle
        rows, err = c.q("select concat(1,2,3,'hello',true, false, to_date('20100309','YYYYMMDD'));")
        check("B2: concat date", rows == [["123hellotf03-09-2010"]], f"rows={rows} err={err}")

        # B3: concat_ws with date
        rows, err = c.q("select concat_ws('#',1,2,3,'hello',true, false, to_date('20100309','YYYYMMDD'));")
        check("B3: concat_ws date", rows == [["1#2#3#hello#t#f#03-09-2010"]], f"rows={rows} err={err}")

        # B4: concat_ws variadic NULL -> NULL
        rows, err = c.q("select concat_ws(',', variadic NULL::int[]);")
        check("B4: concat_ws variadic NULL", rows == [[None]], f"rows={rows} err={err}")

        # B5: concat variadic NULL is NULL
        rows, err = c.q("select concat(variadic NULL::int[]) is NULL;")
        check("B5: concat variadic NULL is null", rows == [["t"]], f"rows={rows} err={err}")

        # B6: concat variadic '{}' -> ''
        rows, err = c.q("select concat(variadic '{}'::int[]) = '';")
        check("B6: concat variadic empty", rows == [["t"]], f"rows={rows} err={err}")

        # B6b: concat variadic with elements
        rows, err = c.q("select concat(variadic '{1,2,3}'::int[]);")
        check("B6b: concat variadic elements", rows == [["123"]], f"rows={rows} err={err}")

        c.close()
    finally:
        proc.terminate()
        proc.wait(timeout=10)

    print(f"\n{PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0

if __name__ == "__main__":
    sys.exit(main())
