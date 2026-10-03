#!/usr/bin/env python3
"""protocol_test85: v0.84 INSERT target indirection (PG19 parity).

Covers the canonical conformance cases from tests/conformance/data/sql/insert.sql:
- `f2[1], f2[2]` array element assignment (VALUES single/multi-row, INSERT...SELECT)
- `f3.if1, f3.if2` composite field assignment
- `f3.if2[1], f3.if2[2]` nested array-in-composite
- `f4[1].if2[1]` deep nesting (array of composites)
- DEFAULT into indirection -> 0A000 with PG19's exact messages
- whole+partial duplicate -> 42701; repeated partial roots allowed
- unknown column / slice / bad subscript errors
- WAL durability of composite arrays (v0.84 fixed the wal.rs Record panic)
"""
import os, socket, struct, subprocess, sys, tempfile, time

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5554

passed, failed = 0, 0
def check(name, cond, detail=""):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL: {name} {detail}")

def msg(t, body):
    return t + struct.pack("!i", 4 + len(body)) + body

class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
        self.drain()
    def drain(self):
        out = []
        while True:
            hdr = self.s.recv(5)
            if len(hdr) < 5: break
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            body = b""
            while len(body) < ln - 4:
                chunk = self.s.recv(ln - 4 - len(body))
                if not chunk: break
                body += chunk
            out.append((typ, body))
            if typ == b"Z": break
        return out
    def q(self, sql):
        self.s.sendall(msg(b"Q", sql.encode() + b"\x00"))
        return self.drain()
    def close(self):
        self.s.sendall(msg(b"X", b""))
        self.s.close()

def errinfo(msgs):
    for typ, body in msgs:
        if typ == b"E":
            code = emsg = None
            pos = 0
            while pos < len(body) - 1:
                f, e = body[pos:pos+1], body.find(b"\x00", pos + 1)
                if e < 0: break
                if f == b"C": code = body[pos+1:e].decode()
                if f == b"M": emsg = body[pos+1:e].decode()
                pos = e + 1
            return code, emsg
    return None, None

def datarows(msgs):
    rows = []
    for typ, body in msgs:
        if typ == b"D":
            n = struct.unpack("!h", body[0:2])[0]
            pos, row = 2, []
            for _ in range(n):
                ln = struct.unpack("!i", body[pos:pos+4])[0]; pos += 4
                if ln == -1: row.append(None)
                else:
                    row.append(body[pos:pos+ln].decode()); pos += ln
            rows.append(row)
    return rows

def start_server(data_dir):
    proc = subprocess.Popen([BIN, "--port", str(PORT), "--data-dir", data_dir],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1); s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("server did not start")

def main():
    dd = tempfile.mkdtemp(prefix="rg_proto85_")
    proc = start_server(dd)
    try:
        c = Conn()
        for sql in ["create type insert_test_type as (if1 int, if2 text[])",
                    "create table inserttest (f1 int, f2 int[], f3 insert_test_type, f4 insert_test_type[])"]:
            ec, em = errinfo(c.q(sql))
            check("setup " + sql[:30], ec is None, f"{ec} {em}")

        # 1. canonical array indirection
        ec, em = errinfo(c.q("insert into inserttest (f2[1], f2[2]) values (1,2)"))
        check("f2[1],f2[2] single row", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f2[1], f2[2]) values (3,4), (5,6)"))
        check("f2[1],f2[2] multi row", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f2[1], f2[2]) select 7,8"))
        check("f2[1],f2[2] insert..select", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select f2 from inserttest where f2 is not null"))
        check("array values", sorted(r[0] for r in rows) == ["{1,2}", "{3,4}", "{5,6}", "{7,8}"],
              f"{rows}")

        # 2. DEFAULT into indirection -> PG19's exact 0A000 messages
        ec, em = errinfo(c.q("insert into inserttest (f2[1], f2[2]) values (1,default)"))
        check("default array elem 0A000", ec == "0A000" and em == "cannot set an array element to DEFAULT",
              f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f3.if1) values (default)"))
        check("default subfield 0A000", ec == "0A000" and em == "cannot set a subfield to DEFAULT",
              f"{ec} {em}")

        # 3. composite field assignment
        ec, em = errinfo(c.q("insert into inserttest (f3.if1, f3.if2) values (1, '{foo}')"))
        check("f3.if1,f3.if2", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select f3 from inserttest where f3 is not null"))
        check("composite value", rows and rows[0][0] == "(1,{foo})", f"{rows}")

        # 4. nested array-in-composite
        ec, em = errinfo(c.q("insert into inserttest (f3.if2[1], f3.if2[2]) values ('baz','quux')"))
        check("f3.if2[1],f3.if2[2]", ec is None, f"{ec} {em}")

        # 5. deep nesting: array of composites
        ec, em = errinfo(c.q("insert into inserttest (f4[1].if2[1], f4[1].if2[2]) values ('a','b')"))
        check("f4[1].if2[1],f4[1].if2[2]", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select f4 from inserttest where f4 is not null"))
        check("nested composite array renders", len(rows) == 1 and rows[0][0] is not None, f"{rows}")

        # 6. duplicates: whole+partial rejected, partial+partial allowed
        ec, em = errinfo(c.q("insert into inserttest (f2, f2[1]) values ('{9}', 9)"))
        check("whole+partial 42701", ec == "42701" and 'column "f2" specified more than once' in (em or ""),
              f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f2, f2) values ('{9}', '{9}')"))
        check("whole+whole 42701", ec == "42701", f"{ec} {em}")

        # 7. error cases
        ec, em = errinfo(c.q("insert into inserttest (nope[1]) values (1)"))
        check("unknown col 42703", ec == "42703", f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f2[1:2]) values (1)"))
        check("slice 0A000", ec == "0A000", f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f1[1]) values (1)"))
        check("subscript non-array 42804", ec == "42804", f"{ec} {em}")
        ec, em = errinfo(c.q("insert into inserttest (f3.nope) values (1)"))
        check("bad field 42703", ec == "42703", f"{ec} {em}")

        # 8. NULL-base growth: index 3 into an empty array creates a
        # [3:3] array (PG19 array_set_element semantics).
        ec, em = errinfo(c.q("insert into inserttest (f2[3]) values (9)"))
        check("f2[3] growth", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select f2 from inserttest where f2[3] = 9"))
        check("grown array", rows and rows[0][0] == "[3:3]={9}", f"{rows}")

        # 9. WAL durability: rows survive a restart (Record tag 18)
        c.q("insert into inserttest (f4[2].if1) values (42)")
        c.close()
        proc.terminate(); proc.wait(timeout=10)
        proc = start_server(dd)
        c = Conn()
        rows = datarows(c.q("select count(*) from inserttest"))
        check("rows survive restart", rows and rows[0][0] not in (None, "0"), f"{rows}")
        rows = datarows(c.q("select f4 from inserttest where f4 is not null"))
        check("composite arrays survive restart", len(rows) >= 2, f"{rows}")
        c.close()
    finally:
        proc.terminate()
    print(f"protocol_test85: {passed} passed, {failed} failed")
    return 1 if failed else 0

sys.exit(main())
