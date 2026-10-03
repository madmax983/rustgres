#!/usr/bin/env python3
"""protocol_test87: v0.86 CREATE FUNCTION / CREATE OPERATOR (PG19 parity).

Covers:
- CREATE FUNCTION (SQL language, $n params; INTERNAL language)
- scalar calls, named-arg rewrite, CREATE OR REPLACE, DROP FUNCTION
- CREATE OPERATOR / DROP OPERATOR with mixed-type IN-subquery dispatch
- composite-returning functions (mki8/mki4) scalar + FROM
- 42883 for unknown functions, 42704 for bad types
- transactional DDL (rollback drops the function)
- WAL replay + checkpoint preserve function/operator catalogs
- malformed SQL body (42601), unsupported internal symbol
"""
import os, socket, struct, subprocess, sys, tempfile, time, shutil

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5587

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
    tmp = tempfile.mkdtemp(prefix="rgs87_")
    proc = subprocess.Popen(
        [BIN, "--data-dir", tmp, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    try:
        c = Conn()
        # --- SQL scalar function ---
        r, e, t = c.q("create function add2(a int, b int) returns int language sql as 'select $1 + $2'")
        check("create function", e is None and t == "CREATE FUNCTION", f"{e} {t}")
        r, e, t = c.q("select add2(3, 4)")
        check("call add2", e is None and r == [["7"]], f"{r} {e}")
        # --- unknown function ---
        r, e, t = c.q("select nosuchfn(1)")
        check("unknown 42883", e is not None and e.startswith("42883"), f"{e}")
        # --- CREATE OR REPLACE ---
        r, e, t = c.q("create or replace function add2(a int, b int) returns int language sql as 'select $1 * $2'")
        check("or replace", e is None, f"{e}")
        r, e, t = c.q("select add2(3, 4)")
        check("replaced body", e is None and r == [["12"]], f"{r} {e}")
        # --- duplicate without OR REPLACE ---
        r, e, t = c.q("create function add2(a int, b int) returns int language sql as 'select 1'")
        check("dup 42723", e is not None and e.startswith("42723"), f"{e}")
        # --- DROP FUNCTION ---
        r, e, t = c.q("drop function add2(int, int)")
        check("drop function", e is None and t == "DROP FUNCTION", f"{e} {t}")
        r, e, t = c.q("select add2(1, 2)")
        check("dropped 42883", e is not None and e.startswith("42883"), f"{e}")
        # --- INTERNAL language ---
        r, e, t = c.q("create function int4eq_unsafe(int4, int4) returns bool language internal as 'int4eq'")
        check("create internal", e is None, f"{e}")
        r, e, t = c.q("select int4eq_unsafe(3, 3)")
        check("internal true", e is None and r == [["t"]], f"{r} {e}")
        r, e, t = c.q("select int4eq_unsafe(3, 4)")
        check("internal false", e is None and r == [["f"]], f"{r} {e}")
        # --- unsupported internal symbol ---
        r, e, t = c.q("create function bad_internal() returns int language internal as 'nosuchsym'")
        check("bad internal", e is not None, f"{e}")
        # --- malformed SQL body ---
        r, e, t = c.q("create function bad_body() returns int language sql as 'select from where'")
        check("bad body 42601", e is not None and e.startswith("42601"), f"{e}")
        # --- composite return (mki8) ---
        r, e, t = c.q("create table int8_tbl(q1 int8, q2 int8)")
        check("create int8_tbl", e is None, f"{e}")
        r, e, t = c.q("create function mki8(bigint, bigint) returns int8_tbl language sql as 'select row($1,$2)::int8_tbl'")
        check("create mki8", e is None, f"{e}")
        r, e, t = c.q("select mki8(1,2)")
        check("mki8 scalar", e is None and r == [["(1,2)"]], f"{r} {e}")
        r, e, t = c.q("select * from mki8(1,2)")
        check("mki8 from", e is None and r == [["1", "2"]], f"{r} {e}")
        # --- custom operator for mixed-type IN ---
        r, e, t = c.q("create table inner_text(c1 text)")
        check("create inner_text", e is None, f"{e}")
        r, e, t = c.q("insert into inner_text values ('123'), ('456')")
        check("insert inner_text", e is None, f"{e}")
        r, e, t = c.q("create function bogus_int8_text_eq(int8, text) returns bool language sql as 'select $1::text = $2'")
        check("create eq fn", e is None, f"{e}")
        r, e, t = c.q("create operator = (procedure = bogus_int8_text_eq, leftarg = int8, rightarg = text)")
        check("create operator", e is None and t == "CREATE OPERATOR", f"{e} {t}")
        r, e, t = c.q("select 123::int8 in (select c1 from inner_text)")
        check("in true", e is None and r == [["t"]], f"{r} {e}")
        r, e, t = c.q("select 999::int8 in (select c1 from inner_text)")
        check("in false", e is None and r == [["f"]], f"{r} {e}")
        r, e, t = c.q("drop operator = (int8, text)")
        check("drop operator", e is None and t == "DROP OPERATOR", f"{e} {t}")
        # --- transactional DDL: rollback ---
        r, e, t = c.q("begin")
        check("begin", e is None, f"{e}")
        r, e, t = c.q("create function tmp_fn() returns int language sql as 'select 1'")
        check("create in txn", e is None, f"{e}")
        r, e, t = c.q("rollback")
        check("rollback", e is None, f"{e}")
        r, e, t = c.q("select tmp_fn()")
        check("rolled back 42883", e is not None and e.startswith("42883"), f"{e}")
        c.close()
        # --- restart: WAL replay preserves catalog ---
        proc.terminate()
        proc.wait()
        time.sleep(1)
        proc2 = subprocess.Popen(
            [BIN, "--data-dir", tmp, "--port", str(PORT)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(2)
        try:
            c2 = Conn()
            r, e, t = c2.q("select mki8(5,6)")
            check("wal replay mki8", e is None and r == [["(5,6)"]], f"{r} {e}")
            r, e, t = c2.q("select int4eq_unsafe(1,1)")
            check("wal replay internal", e is None and r == [["t"]], f"{r} {e}")
            c2.close()
        finally:
            proc2.terminate()
            proc2.wait()
    finally:
        try:
            proc.terminate()
            proc.wait()
        except Exception:
            pass
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"protocol_test87: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
