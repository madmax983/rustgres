#!/usr/bin/env python3
"""protocol_test88: v0.87 quantified comparisons, overloads, DROP CASCADE.

Covers:
- explicit LATERAL table functions
- scalar quantified comparisons: > ANY, > ALL, < ALL with 3VL
- row-wise quantified comparisons: (a,b) > ANY
- row-wise IN-subquery
- user-defined operators: ?= scalar + ?= ANY
- function overloads: CREATE two signatures, call dispatch, DROP one
- CREATE OPERATOR signature validation (42883 on bad procedure sig)
- DROP FUNCTION RESTRICT (2BP01) vs CASCADE (drops operator)
- DROP OPERATOR CASCADE/RESTRICT parse
"""
import os, socket, struct, subprocess, sys, tempfile, time, shutil

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5588

passed, failed = 0, 0
def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL: {name} {detail}")

def msg(t, body):
    return t + struct.pack("!i", 4 + len(body)) + body

class Conn:
    def __init__(self, port=PORT):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
        self.drain()
    def drain(self):
        out = []
        while True:
            hdr = self.s.recv(5)
            if len(hdr) < 5:
                break
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            body = b""
            while len(body) < ln - 4:
                chunk = self.s.recv(ln - 4 - len(body))
                if not chunk:
                    break
                body += chunk
            out.append((typ, body))
            if typ == b"Z":
                break
        return out
    def q(self, sql):
        self.s.sendall(msg(b"Q", sql.encode() + b"\x00"))
        rows, err, tag = [], None, None
        for typ, body in self.drain():
            if typ == b"D":
                n = struct.unpack("!h", body[:2])[0]
                pos, row = 2, []
                for _ in range(n):
                    ln = struct.unpack("!i", body[pos:pos+4])[0]
                    pos += 4
                    if ln == -1:
                        row.append(None)
                    else:
                        row.append(body[pos:pos+ln].decode())
                        pos += ln
                rows.append(row)
            elif typ == b"E":
                f, pos = {}, 0
                while pos < len(body) and body[pos] != 0:
                    c = chr(body[pos])
                    pos += 1
                    e = body.index(b"\x00", pos)
                    f[c] = body[pos:e].decode("utf8", "replace")
                    pos = e + 1
                err = f.get("C", "") + ": " + f.get("M", "")
            elif typ == b"C":
                tag = body[:-1].decode()
        return rows, err, tag
    def close(self):
        self.s.close()

def main():
    tmp = tempfile.mkdtemp(prefix="rgs88_")
    proc = subprocess.Popen(
        [BIN, "--data-dir", tmp, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    try:
        c = Conn()
        # --- explicit LATERAL ---
        r, e, t = c.q("create table lat_q(id int)")
        check("create lat_q", e is None, f"{e}")
        r, e, t = c.q("insert into lat_q values (2), (3)")
        check("insert lat_q", e is None, f"{e}")
        r, e, t = c.q("select q.id, gs.i from lat_q q, lateral generate_series(1, q.id) gs(i) order by 1, 2")
        check("lateral", e is None and r == [["2","1"],["2","2"],["3","1"],["3","2"],["3","3"]], f"{r} {e}")
        # --- scalar quantified ---
        r, e, t = c.q("create table nn(id int, val int)")
        check("create nn", e is None, f"{e}")
        r, e, t = c.q("insert into nn values (1,10),(2,20),(3,30)")
        check("insert nn", e is None, f"{e}")
        r, e, t = c.q("select id from nn where id > any (select id from nn where id > 1) order by 1")
        check("> any", e is None and r == [["3"]], f"{r} {e}")
        r, e, t = c.q("select id from nn where id > all (select id from nn where id < 3) order by 1")
        check("> all", e is None and r == [["3"]], f"{r} {e}")
        r, e, t = c.q("select id from nn where id < all (select id from nn where id > 1) order by 1")
        check("< all empty", e is None and r == [["1"]], f"{r} {e}")
        # --- 3VL: NULL in subquery ---
        r, e, t = c.q("insert into nn values (null, null)")
        check("insert null", e is None, f"{e}")
        r, e, t = c.q("select id from nn where id = any (select id from nn) order by 1")
        # id=1: matches 1 -> true; NULL never matches -> no NULL result for non-null ids
        check("= any 3vl", e is None and r == [["1"],["2"],["3"]], f"{r} {e}")
        r, e, t = c.q("delete from nn where id is null")
        check("delete null", e is None, f"{e}")
        # --- row-wise quantified ---
        r, e, t = c.q("select id from nn where (id, val) > any (select id, val from nn where id < 3) order by 1")
        check("row > any", e is None and r == [["2"],["3"]], f"{r} {e}")
        r, e, t = c.q("select id from nn where not (id, val) > any (select id, val from nn) order by 1")
        check("not row > any", e is None and r == [["1"]], f"{r} {e}")
        # --- row-wise IN ---
        r, e, t = c.q("select id from nn where (id, val) in (select id, val from nn where id = 2)")
        check("row in", e is None and r == [["2"]], f"{r} {e}")
        # --- user-defined operator ---
        r, e, t = c.q("create function int4eq_unsafe(a int, b int) returns bool language sql strict as 'select $1 = $2'")
        check("create int4eq_unsafe", e is None, f"{e}")
        r, e, t = c.q("create operator ?= (leftarg = int4, rightarg = int4, procedure = int4eq_unsafe)")
        check("create ?=", e is None and t == "CREATE OPERATOR", f"{e} {t}")
        r, e, t = c.q("select id from nn where id ?= 2")
        check("?= scalar", e is None and r == [["2"]], f"{r} {e}")
        r, e, t = c.q("select id from nn where not id ?= any (select id from nn where id > 1) order by 1")
        check("not ?= any", e is None and r == [["1"]], f"{r} {e}")
        # --- function overloads ---
        r, e, t = c.q("create function ov(a int) returns int language sql as 'select $1'")
        check("create ov/1", e is None, f"{e}")
        r, e, t = c.q("create function ov(a int, b int) returns int language sql as 'select $1 + $2'")
        check("create ov/2", e is None, f"{e}")
        r, e, t = c.q("select ov(5)")
        check("call ov/1", e is None and r == [["5"]], f"{r} {e}")
        r, e, t = c.q("select ov(5, 6)")
        check("call ov/2", e is None and r == [["11"]], f"{r} {e}")
        # --- duplicate signature without OR REPLACE ---
        r, e, t = c.q("create function ov(a int) returns int language sql as 'select 1'")
        check("dup sig 42723", e is not None and e.startswith("42723"), f"{e}")
        # --- DROP one overload ---
        r, e, t = c.q("drop function ov(int)")
        check("drop ov/1", e is None and t == "DROP FUNCTION", f"{e} {t}")
        r, e, t = c.q("select ov(5, 6)")
        check("ov/2 survives", e is None and r == [["11"]], f"{r} {e}")
        r, e, t = c.q("select ov(5)")
        check("ov/1 gone 42883", e is not None and e.startswith("42883"), f"{e}")
        # --- CREATE OPERATOR signature validation ---
        r, e, t = c.q("create operator ?= (leftarg = text, rightarg = text, procedure = ov)")
        check("bad proc sig 42883", e is not None and e.startswith("42883"), f"{e}")
        # --- DROP FUNCTION RESTRICT vs CASCADE ---
        r, e, t = c.q("drop function int4eq_unsafe(int, int)")
        check("restrict 2BP01", e is not None and e.startswith("2BP01"), f"{e}")
        r, e, t = c.q("drop function int4eq_unsafe(int, int) cascade")
        check("cascade ok", e is None and t == "DROP FUNCTION", f"{e} {t}")
        r, e, t = c.q("select 1 ?= 1")
        check("op cascaded 42883", e is not None and e.startswith("42883"), f"{e}")
        # --- DROP OPERATOR with CASCADE keyword parses ---
        r, e, t = c.q("create function f2(a int, b int) returns bool language sql as 'select $1 = $2'")
        check("create f2", e is None, f"{e}")
        r, e, t = c.q("create operator ?= (leftarg = int4, rightarg = int4, procedure = f2)")
        check("recreate ?=", e is None, f"{e}")
        r, e, t = c.q("drop operator ?= (int4, int4) restrict")
        check("drop op restrict", e is None and t == "DROP OPERATOR", f"{e} {t}")
        c.close()
    finally:
        try:
            proc.terminate()
            proc.wait()
        except Exception:
            pass
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"protocol_test88: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
