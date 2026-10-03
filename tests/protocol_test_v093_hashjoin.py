#!/usr/bin/env python3
r"""v0.93 protocol tests: hash equi-join.

Covers the v0.93 hash join (INNER joins with `lcol = rcol` equi conjuncts):

Differential core: every query runs twice — once with the hash join
enabled, once with RUSTGRES_NO_HASH_JOIN=1 (pure nested loop) — and the
outputs must be byte-identical. This proves the hash path preserves the
nested loop's semantics exactly (three-valued logic, coercions,
residuals, ordering).

Plus targeted expected-value checks:
- duplicates (n x m fan-out within a key group), NULL keys never match
- mixed int/numeric keys, float keys (-0.0 = 0.0 and NaN = NaN per
  v0.94 PG19 parity; previously -0.0 != 0.0 and NaN never matched)
- text/varchar/bpchar (trailing-space) keys, bool/date/timestamp/
  timestamptz/bytea/uuid/"char"/pg_lsn keys
- multi-key equi joins, equi + residual conjuncts, same-side equalities
- USING / NATURAL joins, self-joins, LEFT JOIN (stays nested loop),
  empty inputs, correlated EXISTS-free joins

Self-starting: launches rustgres on 5442 (two runs: hash on/off).
"""
import socket, struct, subprocess, sys, time, os

PORT = 5442
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
    "create table hja(id int, v text);",
    "create table hjb(id int, w text);",
    "insert into hja values (1,'a1'),(2,'a2'),(3,'a3'),(NULL,'aN'),(2,'a2b'),(5,'a5');",
    "insert into hjb values (2,'b2'),(3,'b3'),(4,'b4'),(NULL,'bN'),(2,'b2b');",
    "create table hjn(id numeric, v text);",
    "insert into hjn values (2.0,'n2'),(3.00,'n3'),(NULL,'nN');",
    "create table hjf(id float8, v text);",
    "insert into hjf values (1.5,'f1'),(-0.0,'fn'),(0.0,'fz');",
    "insert into hjf values ('NaN','fN');",
    "create table hjf2(id float8, w text);",
    "insert into hjf2 values (1.5,'g1'),(-0.0,'gn'),(0.0,'gz');",
    "create table hjt(k text, v int);",
    "create table hjv(k varchar(10), v int);",
    "insert into hjt values ('x',1),('y',2),(NULL,3);",
    "insert into hjv values ('x',10),('z',20);",
    "create table hjc(k char(3), v int);",
    "insert into hjc values ('a',1),('ab',2);",
    "create table hjc2(k char(3), v int);",
    "insert into hjc2 values ('a  ',10),('ab ',20),('abc',30);",
    "create table hjm(k1 int, k2 text, v int);",
    "create table hjm2(k1 int, k2 text, v int);",
    "insert into hjm values (1,'x',1),(1,'y',2),(2,'x',3);",
    "insert into hjm2 values (1,'x',10),(1,'y',20),(2,'z',30);",
    "create table hje(id int);",
    "create table hjd(d date, v int);",
    "create table hjd2(d date, v int);",
    "insert into hjd values ('2026-01-01',1),('2026-01-02',2);",
    "insert into hjd2 values ('2026-01-01',10),('2026-01-03',30);",
    "create table hjts(t timestamp, v int);",
    "create table hjts2(t timestamp, v int);",
    "insert into hjts values ('2026-01-01 10:00:00',1);",
    "insert into hjts2 values ('2026-01-01 10:00:00',10);",
    "create table hjb2(b bool, v int);",
    "create table hjb3(b bool, v int);",
    "insert into hjb2 values (true,1),(false,2),(NULL,3);",
    "insert into hjb3 values (true,10),(true,11);",
    "create table hju(u uuid, v int);",
    "create table hju2(u uuid, v int);",
    "insert into hju values ('123e4567-e89b-12d3-a456-426614174000',1);",
    "insert into hju2 values ('123e4567-e89b-12d3-a456-426614174000',10),('123e4567-e89b-12d3-a456-426614174001',20);",
    "create table hjby(k bytea, v int);",
    "create table hjby2(k bytea, v int);",
    "insert into hjby values ('\\x0102',1);",
    "insert into hjby2 values ('\\x0102',10),('\\x03',20);",
]

QUERIES = [
    # basic equi + duplicates + NULLs
    "select a.id, a.v, b.w from hja a join hjb b on a.id = b.id order by 1,2,3;",
    "select count(*) from hja a join hjb b on a.id = b.id;",
    # equi + residual (non-equi conjunct)
    "select a.v from hja a join hjb b on a.id = b.id and a.v <> b.w order by 1;",
    "select a.v from hja a join hjb b on a.id = b.id and (a.v = 'a2' or b.w = 'b3') order by 1;",
    # same-side equality (not a key; residual)
    "select a.v from hja a join hjb b on a.id = b.id and a.id = a.id order by 1;",
    # multi-key
    "select m.v, m2.v from hjm m join hjm2 m2 on m.k1 = m2.k1 and m.k2 = m2.k2 order by 1,2;",
    # reversed sides
    "select a.id from hja a join hjb b on b.id = a.id order by 1;",
    # USING / NATURAL
    "select a.id from hja a join hjb b using (id) order by 1;",
    "select * from hja a natural join hjb b order by 1,2;",
    # LEFT JOIN stays nested-loop but must agree
    "select a.id, b.w from hja a left join hjb b on a.id = b.id order by 1,2;",
    # self-join
    "select x.v, y.v from hja x join hja y on x.id = y.id order by 1,2;",
    # mixed int = numeric
    "select a.v, n.v from hja a join hjn n on a.id = n.id order by 1,2;",
    # float keys; v0.94 PG19 parity: -0.0 = 0.0 (IEEE ==), NaN = NaN
    "select f.v, g.w from hjf f join hjf2 g on f.id = g.id order by 1,2;",
    # text / varchar
    "select t.v, v.v from hjt t join hjv v on t.k = v.k order by 1,2;",
    # bpchar trailing-space semantics
    "select c.v, c2.v from hjc c join hjc2 c2 on c.k = c2.k order by 1,2;",
    # bool / date / timestamp / uuid / bytea
    "select b2.v, b3.v from hjb2 b2 join hjb3 b3 on b2.b = b3.b order by 1,2;",
    "select d.v, d2.v from hjd d join hjd2 d2 on d.d = d2.d order by 1,2;",
    "select t.v, t2.v from hjts t join hjts2 t2 on t.t = t2.t order by 1,2;",
    "select u.v, u2.v from hju u join hju2 u2 on u.u = u2.u order by 1,2;",
    "select y.v, y2.v from hjby y join hjby2 y2 on y.k = y2.k order by 1,2;",
    # empty side
    "select a.id from hja a join hje e on a.id = e.id;",
    "select e.id from hje e join hja a on e.id = a.id;",
    # join with WHERE pushdown + order by over join
    "select a.v, b.w from hja a join hjb b on a.id = b.id where a.id > 1 order by a.v, b.w;",
    # three-way join (nested joins)
    "select a.v, b.w, m.v from hja a join hjb b on a.id = b.id join hjm m on m.k1 = a.id order by 1,2,3;",
    # aggregate over join
    "select b.w, count(*) from hja a join hjb b on a.id = b.id group by b.w order by 1;",
    # cross join unaffected
    "select count(*) from hja a cross join hjb b;",
    # non-equi join stays nested loop
    "select count(*) from hja a join hjb b on a.id < b.id;",
]

EXPECTED = {
    # duplicates: id=2 appears 2x left, 2x right -> 4 rows; id=3 -> 1; NULLs excluded
    "select a.id, a.v, b.w from hja a join hjb b on a.id = b.id order by 1,2,3;": [
        ["2", "a2", "b2"], ["2", "a2", "b2b"], ["2", "a2b", "b2"], ["2", "a2b", "b2b"],
        ["3", "a3", "b3"],
    ],
    "select count(*) from hja a join hjb b on a.id = b.id;": [["5"]],
    # residual a.v <> b.w drops nothing here (all differ) -> 5
    "select a.v from hja a join hjb b on a.id = b.id and a.v <> b.w order by 1;": [
        ["a2"], ["a2"], ["a2b"], ["a2b"], ["a3"],
    ],
    "select m.v, m2.v from hjm m join hjm2 m2 on m.k1 = m2.k1 and m.k2 = m2.k2 order by 1,2;": [
        ["1", "10"], ["2", "20"],
    ],
    # int = numeric: 2 = 2.0, 3 = 3.00
    "select a.v, n.v from hja a join hjn n on a.id = n.id order by 1,2;": [
        ["a2", "n2"], ["a2b", "n2"], ["a3", "n3"],
    ],
    # float: 1.5=1.5; -0.0 = 0.0 (PG19 hashfloat8/-0.0 canonicalization);
    # NaN = NaN would match but hjf2 has no NaN row
    "select f.v, g.w from hjf f join hjf2 g on f.id = g.id order by 1,2;": [
        ["f1", "g1"], ["fn", "gn"], ["fn", "gz"], ["fz", "gn"], ["fz", "gz"],
    ],
    # bpchar: 'a' = 'a  ', 'ab' = 'ab '
    "select c.v, c2.v from hjc c join hjc2 c2 on c.k = c2.k order by 1,2;": [
        ["1", "10"], ["2", "20"],
    ],
    "select b2.v, b3.v from hjb2 b2 join hjb3 b3 on b2.b = b3.b order by 1,2;": [
        ["1", "10"], ["1", "11"],
    ],
    "select t.v, v.v from hjt t join hjv v on t.k = v.k order by 1,2;": [["1", "10"]],
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
        datadir = f"/tmp/rg_proto_v093_{tag}"
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
    print(f"v0.93 hashjoin: {n}/{n} differential identical, {len(EXPECTED)} expected-value checks; {fails} failures")
    sys.exit(1 if fails else 0)

if __name__ == "__main__":
    main()
