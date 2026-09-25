#!/usr/bin/env python3
"""protocol_test86: v0.85 CREATE DOMAIN (PG19 parity).

Covers:
- CREATE DOMAIN with named/unnamed CHECK, NOT NULL, DEFAULT
- unnamed CHECK gets <domain>_check, <domain>_check1, ...
- domain enforcement on table columns (23514, PG19's exact message)
- only FALSE violates; TRUE/NULL pass
- domain DEFAULT on omitted columns and explicit DEFAULT
- DROP DOMAIN [IF EXISTS] [CASCADE|RESTRICT]
- CREATE DOMAIN over composite / array-of-domain
- WAL replay + checkpoint preserve domain definitions
- v0.97: ALTER DOMAIN (ADD/DROP CONSTRAINT, SET/DROP NOT NULL,
  SET/DROP DEFAULT), transactional rollback, pg_typeof domain
  identity, bounded plpgsql (single-RETURN bodies), and durability of
  altered domains + plpgsql functions across restart
"""
import os, socket, struct, subprocess, sys, tempfile, time

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5555

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
    dd = tempfile.mkdtemp(prefix="rg_proto86_")
    proc = start_server(dd)
    try:
        c = Conn()

        # 1. basic domain with named + unnamed CHECK
        ec, em = errinfo(c.q("create domain posint as int check (value > 0)"))
        check("create domain unnamed check", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create domain posint2 as int constraint c_pos check (value > 0)"))
        check("create domain named check", ec is None, f"{ec} {em}")

        # 2. domain on table column: enforcement
        ec, em = errinfo(c.q("create table dt (a posint, b posint2)"))
        check("create table with domains", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt values (1, 2)"))
        check("valid domain insert", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt values (-1, 2)"))
        check("domain violation 23514",
              ec == "23514" and em == 'value for domain posint violates check constraint "posint_check"',
              f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt values (1, -2)"))
        check("named constraint violation",
              ec == "23514" and em == 'value for domain posint2 violates check constraint "c_pos"',
              f"{ec} {em}")

        # 3. NULL passes a CHECK-only domain
        ec, em = errinfo(c.q("insert into dt values (null, null)"))
        check("null passes check-only domain", ec is None, f"{ec} {em}")

        # 4. NOT NULL domain
        ec, em = errinfo(c.q("create domain nnint as int not null"))
        check("create not-null domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create table dt2 (a nnint)"))
        check("table with not-null domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt2 values (null)"))
        check("not-null domain rejects null", ec == "23502", f"{ec} {em}")

        # 5. domain DEFAULT
        ec, em = errinfo(c.q("create domain defint as int default 42"))
        check("create domain with default", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create table dt3 (a int, b defint)"))
        check("table with default domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt3 (a) values (1)"))
        check("omitted col gets domain default", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select b from dt3"))
        check("domain default value", rows == [["42"]], f"{rows}")
        ec, em = errinfo(c.q("insert into dt3 values (2, default)"))
        check("explicit default", ec is None, f"{ec} {em}")

        # 6. domain over composite
        ec, em = errinfo(c.q("create type ct as (x int, y text)"))
        check("create composite", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create domain dct as ct check ((value).x > 0)"))
        check("domain over composite", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create table dt4 (c dct)"))
        check("table with composite domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt4 values (row(1,'a'))"))
        check("valid composite domain insert", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt4 values (row(-1,'a'))"))
        check("composite domain violation", ec == "23514", f"{ec} {em}")

        # 7. array of domain
        ec, em = errinfo(c.q("create domain di as int[] check (value[1] > 0)"))
        check("domain over int array", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create table dt5 (arr di)"))
        check("table with array domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt5 values ('{1,2}')"))
        check("valid array domain insert", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into dt5 values ('{-1,2}')"))
        check("array domain violation", ec == "23514", f"{ec} {em}")

        # 8. DROP DOMAIN
        ec, em = errinfo(c.q("drop domain if exists nosuchdomain"))
        check("drop if exists nonexistent", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("drop domain nosuchdomain"))
        check("drop nonexistent errors", ec is not None, f"{ec} {em}")
        ec, em = errinfo(c.q("create domain tmpd as int"))
        check("create tmp domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("drop domain tmpd"))
        check("drop domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("drop domain tmpd"))
        check("drop twice errors", ec is not None, f"{ec} {em}")

        # 9. direct cast through domain
        ec, em = errinfo(c.q("select 5::posint"))
        check("valid direct cast", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select (-5)::posint"))
        check("invalid direct cast 23514", ec == "23514", f"{ec} {em}")

        # 10. UPDATE enforces domain
        ec, em = errinfo(c.q("update dt set a = -5 where a = 1"))
        check("update enforces domain", ec == "23514", f"{ec} {em}")
        ec, em = errinfo(c.q("update dt set a = 10 where a = 1"))
        check("valid update", ec is None, f"{ec} {em}")

        # 11. durability: checkpoint + restart preserves domains
        ec, em = errinfo(c.q("checkpoint"))
        check("checkpoint", ec is None, f"{ec} {em}")
        c.close()
        proc.terminate(); proc.wait(timeout=10)
        proc = start_server(dd)
        c = Conn()
        ec, em = errinfo(c.q("insert into dt values (-9, 2)"))
        check("domain enforced after restart",
              ec == "23514" and "posint_check" in (em or ""), f"{ec} {em}")
        rows = datarows(c.q("select a from dt order by a"))
        check("data survives restart", len(rows) >= 2, f"{rows}")

        # 12. v0.97: ALTER DOMAIN
        ec, em = errinfo(c.q("create domain adint as int"))
        check("create plain domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint add constraint c_small check (value < 100)"))
        check("alter add named constraint", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select 500::adint"))
        check("added constraint enforced", ec == "23514", f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint add check (value > 0)"))
        check("alter add unnamed constraint", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select (-5)::adint"))
        check("unnamed constraint enforced", ec == "23514", f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint drop constraint c_small"))
        check("alter drop constraint", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select 500::adint"))
        check("dropped constraint not enforced", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint drop constraint c_small"))
        check("drop missing constraint 42704", ec == "42704", f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint drop constraint if exists c_small"))
        check("drop if exists silent", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint add constraint c_small check (value < 100)"))
        check("re-add after drop", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint add constraint c_small check (value < 50)"))
        check("duplicate constraint name 42710", ec == "42710", f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain nosuchdomain set not null"))
        check("alter missing domain 42704", ec == "42704", f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint set not null"))
        check("alter set not null", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select null::adint"))
        check("not null enforced 23502", ec == "23502", f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint drop not null"))
        check("alter drop not null", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select null::adint"))
        check("null ok after drop not null", rows == [[None]], f"{rows}")
        ec, em = errinfo(c.q("alter domain adint set default 7"))
        check("alter set default", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create table adt (x adint)"))
        check("table on altered domain", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("insert into adt default values"))
        check("insert default", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select x from adt"))
        check("altered default applied", rows == [["7"]], f"{rows}")
        ec, em = errinfo(c.q("alter domain adint drop default"))
        check("alter drop default", ec is None, f"{ec} {em}")
        # transactional: rollback restores
        ec, em = errinfo(c.q("begin"))
        check("begin", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("alter domain adint add constraint c_tmp check (value < 10)"))
        check("add in txn", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select 50::adint"))
        check("txn constraint enforced", ec == "23514", f"{ec} {em}")
        ec, em = errinfo(c.q("rollback"))
        check("rollback", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("select 50::adint"))
        check("rollback restores definition", ec is None, f"{ec} {em}")

        # 13. v0.97: pg_typeof reports domains
        rows = datarows(c.q("select pg_typeof(5::adint)"))
        check("pg_typeof cast to domain", rows == [["adint"]], f"{rows}")
        rows = datarows(c.q("select pg_typeof(x) from adt"))
        check("pg_typeof domain column", rows == [["adint"]], f"{rows}")
        rows = datarows(c.q("select pg_typeof(5), pg_typeof(null)"))
        check("pg_typeof base types unchanged", rows == [["integer", "unknown"]], f"{rows}")

        # 14. v0.97: bounded plpgsql (single-RETURN bodies)
        ec, em = errinfo(c.q(
            "create function vol(text) returns text as 'begin return $1; end' language plpgsql volatile"))
        check("create plpgsql function", ec is None, f"{ec} {em}")
        rows = datarows(c.q("select vol('hello')"))
        check("plpgsql call", rows == [["hello"]], f"{rows}")
        rows = datarows(c.q("select vol('a') || vol('b')"))
        check("plpgsql volatile repeat call", rows == [["ab"]], f"{rows}")
        ec, em = errinfo(c.q(
            "create function badpl() returns int as 'begin x := 1; return 1; end' language plpgsql"))
        check("rich plpgsql body 0A000", ec == "0A000", f"{ec} {em}")
        # the foodomain cascade from pg_regress domains: plpgsql no
        # longer aborts the txn, so CREATE DOMAIN succeeds
        ec, em = errinfo(c.q("begin"))
        check("begin 2", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q(
            "create function vol2(text) returns text as 'begin return $1; end' language plpgsql volatile"))
        check("create plpgsql in txn", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("create domain foodomain as text"))
        check("domain after plpgsql in txn", ec is None, f"{ec} {em}")
        ec, em = errinfo(c.q("commit"))
        check("commit", ec is None, f"{ec} {em}")

        # 15. durability: altered domains + plpgsql functions survive restart
        ec, em = errinfo(c.q("checkpoint"))
        check("checkpoint 2", ec is None, f"{ec} {em}")
        c.close()
        proc.terminate(); proc.wait(timeout=10)
        proc = start_server(dd)
        c = Conn()
        ec, em = errinfo(c.q("select 500::adint"))
        check("altered domain enforced after restart", ec == "23514", f"{ec} {em}")
        rows = datarows(c.q("select vol('hi')"))
        check("plpgsql function survives restart", rows == [["hi"]], f"{rows}")

        c.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=10)
        except subprocess.TimeoutExpired: proc.kill()

    print(f"protocol_test86: {passed} passed, {failed} failed")
    return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(main())
