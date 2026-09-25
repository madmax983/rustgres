#!/usr/bin/env python3
"""protocol_test89: v0.88 index DDL (DESC/NULLS, expression, partial,
multi-name DROP) + virtual pg_attribute.

Covers:
- CREATE INDEX with DESC / NULLS FIRST|LAST key options (single + multi-key)
- expression indexes: accepted as catalog-only (no build, no planner use)
- partial indexes (WHERE): accepted as catalog-only
- DROP INDEX with comma-separated names; missing name -> 42P01
- DML with catalog-only indexes present (no panic, no enforcement)
- plain UNIQUE indexes still enforced; ORDER BY correct with DESC index
- virtual pg_attribute: attrelid/attname/attnum + the conformance
  RIGHT JOIN introspection query
- transaction rollback of CREATE INDEX
- restart durability: checkpoint + restart preserves the new index metadata
"""
import os, socket, struct, subprocess, sys, tempfile, time, shutil

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5589

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

def start_server(tmp):
    proc = subprocess.Popen(
        [BIN, "--data-dir", tmp, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    return proc

def main():
    tmp = tempfile.mkdtemp(prefix="rgs89_")
    proc = start_server(tmp)
    try:
        c = Conn()
        # --- DESC / NULLS key options ---
        r, e, t = c.q("create table idx88 (a int, b int)")
        check("create idx88", e is None, f"{e}")
        r, e, t = c.q("insert into idx88 values (3, 1), (1, 2), (2, 3)")
        check("insert idx88", e is None, f"{e}")
        r, e, t = c.q("create index idx88_desc on idx88 (a desc)")
        check("desc index", e is None and t == "CREATE INDEX", f"{e} {t}")
        r, e, t = c.q("create index idx88_nf on idx88 (b desc nulls first)")
        check("desc nulls first", e is None and t == "CREATE INDEX", f"{e} {t}")
        r, e, t = c.q("create index idx88_m on idx88 (a asc, b desc nulls last)")
        check("multi-key direction", e is None and t == "CREATE INDEX", f"{e} {t}")
        r, e, t = c.q("select a from idx88 order by a")
        check("order asc", e is None and r == [["1"], ["2"], ["3"]], f"{r} {e}")
        r, e, t = c.q("select a from idx88 order by a desc")
        check("order desc", e is None and r == [["3"], ["2"], ["1"]], f"{r} {e}")
        # --- expression index: catalog-only ---
        r, e, t = c.q("create unique index idx88_fn on idx88 ((a * a))")
        check("expr index", e is None and t == "CREATE INDEX", f"{e} {t}")
        # DML must not panic and must not enforce the catalog-only index.
        r, e, t = c.q("insert into idx88 values (2, 9)")
        check("insert with expr idx", e is None, f"{e}")
        r, e, t = c.q("update idx88 set b = 7 where a = 1")
        check("update with expr idx", e is None, f"{e}")
        r, e, t = c.q("delete from idx88 where a = 2 and b = 9")
        check("delete with expr idx", e is None, f"{e}")
        # --- partial index: catalog-only ---
        r, e, t = c.q("create index idx88_part on idx88 (b) where b > 1")
        check("partial index", e is None and t == "CREATE INDEX", f"{e} {t}")
        r, e, t = c.q("insert into idx88 values (4, 4)")
        check("insert with partial idx", e is None, f"{e}")
        # --- plain unique index still enforced ---
        r, e, t = c.q("create unique index idx88_u on idx88 (a)")
        check("unique index", e is None and t == "CREATE INDEX", f"{e} {t}")
        r, e, t = c.q("insert into idx88 values (1, 5)")
        check("unique enforced", e is not None and e.startswith("23505"), f"{e}")
        # --- multi-name DROP INDEX ---
        r, e, t = c.q("drop index idx88_desc, idx88_nf, idx88_m")
        check("multi drop", e is None and t == "DROP INDEX", f"{e} {t}")
        r, e, t = c.q("drop index idx88_missing")
        check("missing 42P01", e is not None and e.startswith("42P01"), f"{e}")
        r, e, t = c.q("drop index idx88_fn, idx88_part, idx88_u")
        check("drop catalog-only", e is None and t == "DROP INDEX", f"{e} {t}")
        # --- pg_attribute ---
        r, e, t = c.q("select attname, attnum from pg_attribute where attname = 'a' and attrelid = (select oid from pg_class where relname = 'idx88') order by attnum")
        check("pg_attribute", e is None and r == [["a", "1"]], f"{r} {e}")
        r, e, t = c.q(
            "select tname, attname from ("
            " select relname::information_schema.sql_identifier as tname, *"
            " from (select * from pg_class c) ss1) ss2"
            " right join pg_attribute a on a.attrelid = ss2.oid"
            " where tname = 'idx88' and attnum = 1")
        check("pg_attribute right join", e is None and r == [["idx88", "a"]], f"{r} {e}")
        # --- rollback of CREATE INDEX ---
        r, e, t = c.q("begin")
        check("begin", e is None, f"{e}")
        r, e, t = c.q("create index idx88_rb on idx88 (a desc) where a > 0")
        check("create in txn", e is None, f"{e}")
        r, e, t = c.q("rollback")
        check("rollback", e is None and t == "ROLLBACK", f"{e} {t}")
        r, e, t = c.q("drop index idx88_rb")
        check("rolled back 42P01", e is not None and e.startswith("42P01"), f"{e}")
        # --- restart durability ---
        r, e, t = c.q("create index idx88_d on idx88 (a desc nulls first)")
        check("recreate desc", e is None, f"{e}")
        r, e, t = c.q("create unique index idx88_e on idx88 ((b + 1))")
        check("recreate expr", e is None, f"{e}")
        r, e, t = c.q("checkpoint")
        check("checkpoint", e is None and t == "CHECKPOINT", f"{e} {t}")
        c.close()
        proc.terminate(); proc.wait()
        proc = start_server(tmp)
        c = Conn()
        r, e, t = c.q("select a from idx88 order by a desc")
        check("post-restart order", e is None and r == [["4"], ["3"], ["2"], ["1"]], f"{r} {e}")
        r, e, t = c.q("drop index idx88_d, idx88_e")
        check("post-restart drop", e is None and t == "DROP INDEX", f"{e} {t}")
        r, e, t = c.q("insert into idx88 values (5, 5)")
        check("post-restart insert", e is None, f"{e}")
        c.close()
    finally:
        try:
            proc.terminate()
            proc.wait()
        except Exception:
            pass
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"protocol_test89: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
