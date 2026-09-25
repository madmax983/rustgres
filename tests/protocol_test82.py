#!/usr/bin/env python3
"""protocol_test82: v0.81 composite types — CREATE TYPE AS, ROW(), field access, *=.

Covers the PG19 subselect.sql composite block:
  CREATE TYPE t_rec AS (x numeric);
  CREATE TEMP TABLE pdt (id int, a t_rec);
  INSERT INTO pdt VALUES (1, ROW(1.00)::t_rec), (2, ROW(1.0)::t_rec), (3, ROW(2)::t_rec);
"""
import os, socket, struct, subprocess, sys, tempfile, time

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5433

passed, failed = 0, 0
def check(name, cond, detail=""):
    global passed, failed
    if cond: passed += 1
    else:
        failed += 1
        print(f"FAIL: {name} {detail}")

class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        # Startup message
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
        # Read until ReadyForQuery
        self._read_until(b"Z")
    def _read_until(self, want):
        buf = b""
        while True:
            hdr = self.s.recv(5)
            if len(hdr) < 5: break
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            body = b""
            while len(body) < ln - 4:
                chunk = self.s.recv(ln - 4 - len(body))
                if not chunk: break
                body += chunk
            buf += typ + body
            if typ == want: break
        return buf
    def query(self, sql):
        q = b"Q" + struct.pack("!i", 4 + len(sql) + 1) + sql.encode() + b"\x00"
        self.s.sendall(q)
        rows, cols, err = [], [], None
        while True:
            hdr = self.s.recv(5)
            if len(hdr) < 5: break
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            body = b""
            while len(body) < ln - 4:
                chunk = self.s.recv(ln - 4 - len(body))
                if not chunk: break
                body += chunk
            if typ == b"T":
                n = struct.unpack("!h", body[0:2])[0]
                pos = 2
                for _ in range(n):
                    e = body.index(b"\x00", pos)
                    cols.append(body[pos:e].decode())
                    pos = e + 19
            elif typ == b"D":
                n = struct.unpack("!h", body[0:2])[0]
                pos = 2
                row = []
                for _ in range(n):
                    ln2 = struct.unpack("!i", body[pos:pos+4])[0]
                    pos += 4
                    if ln2 == -1: row.append(None)
                    else:
                        row.append(body[pos:pos+ln2].decode())
                        pos += ln2
                rows.append(row)
            elif typ == b"E":
                # Error: find code (field 'C')
                pos = 0
                while pos < len(body) - 1:
                    f = body[pos:pos+1]
                    e = body.find(b"\x00", pos + 1)
                    if e < 0: break
                    if f == b"C": err = body[pos+1:e].decode()
                    pos = e + 1
            elif typ == b"Z":
                break
        return {"rows": rows, "cols": cols, "err": err}
    def close(self):
        self.s.sendall(b"X" + struct.pack("!i", 4))
        self.s.close()

def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", data_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return proc
        except OSError:
            time.sleep(0.1)
    proc.terminate()
    raise RuntimeError("server did not start")

def stop_server(proc):
    proc.terminate()
    proc.wait()

def main():
    data_dir = tempfile.mkdtemp(prefix="rg82_")
    proc = start_server(data_dir)
    try:
        c = Conn()
        # -- Setup --
        m = c.query("CREATE TYPE t_rec AS (x numeric)")
        check("A1 CREATE TYPE AS", m["err"] is None, m["err"])
        m = c.query("CREATE TEMP TABLE pdt (id int, a t_rec)")
        check("A2 CREATE TEMP TABLE with composite", m["err"] is None, m["err"])
        m = c.query("INSERT INTO pdt VALUES (1, ROW(1.00)::t_rec), (2, ROW(1.0)::t_rec), (3, ROW(2)::t_rec)")
        check("A3 INSERT ROW()::t_rec", m["err"] is None, m["err"])

        # -- Basic SELECT --
        m = c.query("SELECT id, a FROM pdt ORDER BY id")
        check("B1 SELECT composites", m["err"] is None and m["rows"] == [["1", "(1.00)"], ["2", "(1.0)"], ["3", "(2)"]], (m["err"], m["rows"]))

        # -- Field access --
        m = c.query("SELECT (a).x FROM pdt ORDER BY id")
        check("C1 (a).x", m["err"] is None and m["rows"] == [["1.00"], ["1.0"], ["2"]], (m["err"], m["rows"]))

        # -- ROW() --
        m = c.query("SELECT ROW(1, 'a', true)")
        check("D1 ROW() literal", m["err"] is None and m["rows"] == [["(1,a,t)"]], (m["err"], m["rows"]))
        m = c.query("SELECT (ROW(1,2)).f1, (ROW(1,2)).f2")
        check("D2 (ROW()).fN", m["err"] is None and m["rows"] == [["1", "2"]], (m["err"], m["rows"]))

        # -- Semantic = --
        m = c.query("SELECT id FROM pdt WHERE a = ROW(1.0)::t_rec ORDER BY id")
        check("E1 = semantic", m["err"] is None and m["rows"] == [["1"], ["2"]], (m["err"], m["rows"]))

        # -- Image *= --
        m = c.query("SELECT id FROM pdt WHERE a *= ROW(1.0)::t_rec ORDER BY id")
        check("F1 *= image (only 1.0)", m["err"] is None and m["rows"] == [["2"]], (m["err"], m["rows"]))
        m = c.query("SELECT id FROM pdt WHERE a *= ROW(1.00)::t_rec ORDER BY id")
        check("F2 *= image (only 1.00)", m["err"] is None and m["rows"] == [["1"]], (m["err"], m["rows"]))

        # -- DISTINCT ON --
        m = c.query("SELECT DISTINCT ON ((a).x) a, id FROM pdt ORDER BY (a).x, id")
        check("G1 DISTINCT ON", m["err"] is None and len(m["rows"]) == 2, (m["err"], m["rows"]))

        # -- Errors --
        m = c.query("SELECT 1::bogus_type_xyz")
        check("H1 unknown cast -> 42704", m["err"] == "42704", m["err"])
        m = c.query("CREATE TEMP TABLE t2 (x nosuchtype)")
        check("H2 unknown col type -> 42704", m["err"] == "42704", m["err"])

        c.close()
    finally:
        stop_server(proc)
    print(f"protocol_test82: {passed} passed, {failed} failed")
    return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(main())
