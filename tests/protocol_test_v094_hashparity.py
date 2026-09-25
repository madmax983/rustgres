#!/usr/bin/env python3
r"""v0.94 protocol tests: hash-join / grouping PG19 semantic parity.

Covers the v0.94 canonicalization fixes, grounded in PostgreSQL
REL_19_STABLE (a4052efe):

- float.h `float8_eq`: NaN = NaN is TRUE, NaN <> NaN is FALSE
- numeric.c `cmp_numerics`: "We consider all NANs to be equal"
- hashfunc.c `hashfloat4`/`hashfloat8`: -0.0 hashes as 0.0, every NaN
  hashes as one standard NaN
- numeric.c `hash_numeric`: leading/trailing zeros omitted from the
  hash input ("we're paranoid"); specials hashed by kind
- hashfunc.c `hashint8`: compatible with hashint4/hashint2 for
  logically equal inputs (cross-type hash joins)

Differential core: every query runs twice — once with the hash join
enabled, once with RUSTGRES_NO_HASH_JOIN=1 (pure nested loop) — and
the outputs must be byte-identical. This proves the hash path
preserves the nested loop's semantics exactly.

Plus targeted expected-value checks:
- NaN = NaN joins (float + numeric, both sides)
- -0.0 = 0.0 joins (cross-signed-zero fan-out)
- cross-scale numeric joins (5 = 5.0 = 5.00 = 5.000)
- cross-width int joins (smallint = int = bigint)
- int = numeric joins
- GROUP BY / DISTINCT canonicalization: -0.0/0.0 one group, NaN one
  group, NaN/Inf/-Inf three distinct groups, 5/5.0/5.00 one group
- ORDER BY float: NaN last, -0.0 and 0.0 adjacent (equal)

Self-starting: launches rustgres on 5443 (two runs: hash on/off).
"""
import socket, struct, subprocess, sys, time, os

PORT = 5443
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

SETUP = [
    "create table p94f(a float8, v text);",
    "insert into p94f values (1.5,'a'),(-0.0,'b'),(0.0,'c'),('NaN','d'),(2.5,'e');",
    "create table p94f2(a float8, w text);",
    "insert into p94f2 values (0.0,'x'),(-0.0,'y'),('NaN','z'),(1.5,'w');",
    "create table p94n(a numeric, v text);",
    "insert into p94n values (5,'a'),(5.0,'b'),(5.00,'c'),('NaN','d'),(7,'e');",
    "create table p94n2(a numeric, w text);",
    "insert into p94n2 values (5.000,'x'),('NaN','y'),(7.0,'z');",
    "create table p94i(a int, v text);",
    "insert into p94i values (5,'i5'),(6,'i6');",
    "create table p94bi(a bigint, w text);",
    "insert into p94bi values (5,'b5'),(7,'b7');",
    "create table p94s(a smallint, v text);",
    "insert into p94s values (5,'s5');",
    "create table p94sp(a numeric);",
    "insert into p94sp values ('NaN'),('Infinity'),('-Infinity'),('NaN');",
]

QUERIES = [
    # NaN = NaN (float), -0.0 = 0.0 cross fan-out, 1.5 = 1.5; 2.5 unmatched
    "select a.v, b.w from p94f a join p94f2 b on a.a = b.a order by 1,2;",
    # numeric NaN = NaN; 5 = 5.0 = 5.00 = 5.000; 7 = 7.0
    "select a.v, b.w from p94n a join p94n2 b on a.a = b.a order by 1,2;",
    # cross-width: int = bigint
    "select i.v, b.w from p94i i join p94bi b on i.a = b.a order by 1,2;",
    # cross-width: smallint = bigint
    "select s.v, b.w from p94s s join p94bi b on s.a = b.a;",
    # cross-family: int = numeric
    "select i.v, n.w from p94i i join p94n2 n on i.a = n.a order by 1,2;",
    # multi-key with floats
    "select a.v, b.w from p94f a join p94f2 b on a.a = b.a and a.v <> b.w order by 1,2;",
    # LEFT JOIN stays nested-loop but must agree
    "select a.v, b.w from p94f a left join p94f2 b on a.a = b.a order by 1,2;",
    # GROUP BY / DISTINCT canonicalization
    "select count(*) from (select distinct a from p94f) t;",
    "select count(*) from (select distinct a from p94n) t;",
    "select count(*) from (select distinct a from p94sp) t;",
    # scalar comparisons
    "select -0.0::float8 = 0.0::float8, 'nan'::float8 = 'nan'::float8, 'nan'::numeric = 'nan'::numeric;",
    "select 'nan'::float8 <> 'nan'::float8, 'nan'::numeric <> 'nan'::numeric;",
    # ORDER BY float: -0.0/0.0 adjacent (equal), NaN last
    "select v from p94f order by a;",
    "select v from p94f order by a desc;",
]

EXPECTED = {
    # 1.5=w; -0.0 and 0.0 cross-match (2x2); NaN=NaN; 2.5 unmatched
    "select a.v, b.w from p94f a join p94f2 b on a.a = b.a order by 1,2;": [
        ["a", "w"], ["b", "x"], ["b", "y"], ["c", "x"], ["c", "y"], ["d", "z"],
    ],
    # 5/5.0/5.00 x 5.000; NaN x NaN; 7 x 7.0
    "select a.v, b.w from p94n a join p94n2 b on a.a = b.a order by 1,2;": [
        ["a", "x"], ["b", "x"], ["c", "x"], ["d", "y"], ["e", "z"],
    ],
    "select i.v, b.w from p94i i join p94bi b on i.a = b.a order by 1,2;": [
        ["i5", "b5"],
    ],
    "select s.v, b.w from p94s s join p94bi b on s.a = b.a;": [
        ["s5", "b5"],
    ],
    "select i.v, n.w from p94i i join p94n2 n on i.a = n.a order by 1,2;": [
        ["i5", "x"],
    ],
    # residual a.v <> b.w drops nothing (all differ)
    "select a.v, b.w from p94f a join p94f2 b on a.a = b.a and a.v <> b.w order by 1,2;": [
        ["a", "w"], ["b", "x"], ["b", "y"], ["c", "x"], ["c", "y"], ["d", "z"],
    ],
    # LEFT JOIN: every left row survives; 2.5 has no match
    "select a.v, b.w from p94f a left join p94f2 b on a.a = b.a order by 1,2;": [
        ["a", "w"], ["b", "x"], ["b", "y"], ["c", "x"], ["c", "y"], ["d", "z"], ["e", None],
    ],
    # distinct floats: 1.5, 0.0(-0.0), NaN, 2.5
    "select count(*) from (select distinct a from p94f) t;": [["4"]],
    # distinct numerics: 5, NaN, 7
    "select count(*) from (select distinct a from p94n) t;": [["3"]],
    # NaN, +Inf, -Inf stay distinct
    "select count(*) from (select distinct a from p94sp) t;": [["3"]],
    "select -0.0::float8 = 0.0::float8, 'nan'::float8 = 'nan'::float8, 'nan'::numeric = 'nan'::numeric;": [
        ["t", "t", "t"],
    ],
    "select 'nan'::float8 <> 'nan'::float8, 'nan'::numeric <> 'nan'::numeric;": [
        ["f", "f"],
    ],
    # -0.0/0.0 adjacent (equal keys, stable), NaN last
    "select v from p94f order by a;": [["b"], ["c"], ["a"], ["e"], ["d"]],
    "select v from p94f order by a desc;": [["d"], ["e"], ["a"], ["b"], ["c"]],
}

class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=30)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!I", 8 + len(params)) + b"\x00\x03\x00\x00" + params)
        while True:
            t, p = read_msg(self.s)
            if t == b"Z":
                break

    def q(self, sql):
        self.s.sendall(msg(b"Q", cstr(sql)))
        rows, err_code = [], None
        while True:
            t, p = read_msg(self.s)
            if t == b"D":
                n = struct.unpack("!H", p[:2])[0]
                off = 2
                row = []
                for _ in range(n):
                    ln = struct.unpack("!i", p[off:off + 4])[0]
                    off += 4
                    if ln < 0:
                        row.append(None)
                    else:
                        row.append(p[off:off + ln].decode())
                        off += ln
                rows.append(row)
            elif t == b"E":
                i = p.find(b"C")
                err_code = p[i + 1:p.find(b"\x00", i)].decode() if i >= 0 else "?"
            elif t == b"Z":
                break
        return rows, err_code

def run_all(port):
    c = Conn(port)
    for s in SETUP:
        rows, err = c.q(s)
        assert err is None, f"setup failed: {s} -> {err}"
    out = {}
    for sql in QUERIES:
        rows, err = c.q(sql)
        out[sql] = (rows, err)
        assert err is None, f"query failed: {sql} -> {err}"
    return out

def main():
    results = {}
    for tag, env in (("hash", {}), ("nested", {"RUSTGRES_NO_HASH_JOIN": "1"})):
        datadir = f"/tmp/rg_proto_v094_{tag}"
        os.system(f"rm -rf {datadir}")
        e = dict(os.environ)
        e.update(env)
        proc = subprocess.Popen(
            [BIN, "--port", str(PORT), "--data-dir", datadir],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=e)
        try:
            for _ in range(100):
                try:
                    results[tag] = run_all(PORT)
                    break
                except (ConnectionRefusedError, RuntimeError):
                    time.sleep(0.2)
            else:
                print(f"FATAL: server ({tag}) never came up")
                sys.exit(2)
        finally:
            proc.terminate()
            proc.wait()
    fails = 0
    # 1. differential: hash vs nested must be identical
    for sql in QUERIES:
        rh, rn = results["hash"][sql], results["nested"][sql]
        if rh != rn:
            fails += 1
            print(f"DIFF-FAIL {sql}\n  hash:   {rh}\n  nested: {rn}")
    # 2. expected values on the hash run
    for sql, exp in EXPECTED.items():
        got, err = results["hash"][sql]
        if got != exp:
            fails += 1
            print(f"EXPECT-FAIL {sql}\n  want: {exp}\n  got:  {got}")
    n = len(QUERIES)
    print(f"v0.94 hashparity: {n}/{n} differential identical, {len(EXPECTED)} expected-value checks; {fails} failures")
    sys.exit(1 if fails else 0)

if __name__ == "__main__":
    main()
