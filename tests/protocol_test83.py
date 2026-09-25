#!/usr/bin/env python3
"""protocol_test83: v0.82 composite follow-through.

Covers, over the wire:
  A. Composite text input: '(1,foo)'::mytype, quoting/escapes, NULL
     fields, malformed input -> 22P02, nested composites.
  B. Record ORDER BY: lexicographic, NULL fields sort largest, nested.
  C. Scalar *= -> 42883 (PG has no *= for scalars); record *= still image.
  D. CREATE TYPE durability: survives plain restart (WAL replay) and
     CHECKPOINT + restart; DROP TYPE survives restart too.
"""
import os, socket, struct, subprocess, sys, tempfile, time

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
PORT = 5553

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
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
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
    data_dir = tempfile.mkdtemp(prefix="rg83_")
    proc = start_server(data_dir)
    try:
        c = Conn()
        # -- Setup --
        m = c.query("CREATE TYPE t83 AS (x int, y text)")
        check("A0 CREATE TYPE t83", m["err"] is None, m["err"])

        # -- A. Composite text input --
        m = c.query("SELECT '(1,foo)'::t83")
        check("A1 basic text input", m["err"] is None and m["rows"] == [["(1,foo)"]], (m["err"], m["rows"]))
        m = c.query("SELECT '(1,foo)'::t83 = ROW(1,'foo')::t83")
        check("A2 text = ROW()", m["err"] is None and m["rows"] == [["t"]], (m["err"], m["rows"]))
        m = c.query("SELECT ('(1,)'::t83).x, (('(1,)'::t83)).y IS NULL")
        check("A3 trailing empty field is NULL", m["err"] is None and m["rows"] == [["1", "t"]], (m["err"], m["rows"]))
        m = c.query("SELECT (('(,foo)'::t83)).x IS NULL")
        check("A4 leading empty field is NULL", m["err"] is None and m["rows"] == [["t"]], (m["err"], m["rows"]))
        m = c.query("SELECT '(1,\"a,b\")'::t83")
        check("A5 quoted field with comma", m["err"] is None and m["rows"] == [["(1,\"a,b\")"]], (m["err"], m["rows"]))
        m = c.query("SELECT '(1)'::t83")
        check("A6 too few -> 22P02", m["err"] == "22P02", m["err"])
        m = c.query("SELECT '(1,2,3)'::t83")
        check("A7 too many -> 22P02", m["err"] == "22P02", m["err"])
        m = c.query("SELECT '(x,foo)'::t83")
        check("A8 bad int -> 22P02", m["err"] == "22P02", m["err"])
        m = c.query("SELECT '1,foo'::t83")
        check("A9 missing parens -> 22P02", m["err"] == "22P02", m["err"])

        # Nested composite text input
        m = c.query("CREATE TYPE t83in AS (a int)")
        check("A10 CREATE TYPE t83in", m["err"] is None, m["err"])
        m = c.query("CREATE TYPE t83out AS (i t83in, b text)")
        check("A11 CREATE TYPE t83out (nested)", m["err"] is None, m["err"])
        m = c.query("SELECT '((5),hi)'::t83out")
        check("A12 nested text input", m["err"] is None and m["rows"] == [["(\"(5)\",hi)"]], (m["err"], m["rows"]))
        m = c.query("SELECT ('(\"(5)\",hi)'::t83out) = '((5),hi)'::t83out")
        check("A12b nested round-trip", m["err"] is None and m["rows"] == [["t"]], (m["err"], m["rows"]))

        # -- B. Record ORDER BY --
        m = c.query("CREATE TEMP TABLE rt83 (r t83)")
        check("B0 temp table", m["err"] is None, m["err"])
        m = c.query("INSERT INTO rt83 VALUES (ROW(2,'b')::t83), (ROW(1,'a')::t83), (ROW(1,'c')::t83), (ROW(NULL,'z')::t83)")
        check("B1 insert rows", m["err"] is None, m["err"])
        m = c.query("SELECT (r).x, (r).y FROM rt83 ORDER BY r")
        check("B2 record ORDER BY asc", m["err"] is None and m["rows"] == [["1","a"],["1","c"],["2","b"],[None,"z"]], (m["err"], m["rows"]))
        m = c.query("SELECT (r).x FROM rt83 ORDER BY r DESC")
        check("B3 record ORDER BY desc", m["err"] is None and m["rows"] == [[None],["2"],["1"],["1"]], (m["err"], m["rows"]))

        # -- C. Scalar *= -> 42883 --
        m = c.query("SELECT 1 *= 2")
        check("C1 int *= -> 42883", m["err"] == "42883", m["err"])
        m = c.query("SELECT 'a' *= 'a'")
        check("C2 text *= -> 42883", m["err"] == "42883", m["err"])
        m = c.query("SELECT NULL *= 1")
        check("C3 null *= -> 42883", m["err"] == "42883", m["err"])
        m = c.query("SELECT ROW(1,'a')::t83 *= ROW(1,'a')::t83")
        check("C4 record *= still image", m["err"] is None and m["rows"] == [["t"]], (m["err"], m["rows"]))
        m = c.query("SELECT ROW(1,'a')::t83 *= ROW(1,'A')::t83")
        check("C5 record *= image-ne", m["err"] is None and m["rows"] == [["f"]], (m["err"], m["rows"]))
        c.close()

        # -- D. Durability: plain restart (WAL replay, no checkpoint) --
        stop_server(proc)
        proc = start_server(data_dir)
        c = Conn()
        m = c.query("SELECT '(1,foo)'::t83")
        check("D1 type survives restart (WAL)", m["err"] is None and m["rows"] == [["(1,foo)"]], (m["err"], m["rows"]))
        m = c.query("SELECT '((5),hi)'::t83out")
        check("D2 nested type survives restart", m["err"] is None and m["rows"] == [["(\"(5)\",hi)"]], (m["err"], m["rows"]))
        # Checkpoint + restart
        m = c.query("CHECKPOINT")
        check("D3 CHECKPOINT", m["err"] is None, m["err"])
        c.close()
        stop_server(proc)
        proc = start_server(data_dir)
        c = Conn()
        m = c.query("SELECT '(2,bar)'::t83")
        check("D4 type survives CHECKPOINT+restart", m["err"] is None and m["rows"] == [["(2,bar)"]], (m["err"], m["rows"]))
        # DROP TYPE survives restart
        m = c.query("CREATE TYPE t83drop AS (q int)")
        check("D5 CREATE TYPE t83drop", m["err"] is None, m["err"])
        m = c.query("DROP TYPE t83drop")
        check("D6 DROP TYPE t83drop", m["err"] is None, m["err"])
        c.close()
        stop_server(proc)
        proc = start_server(data_dir)
        c = Conn()
        m = c.query("SELECT 1::t83drop")
        check("D7 dropped type stays dropped", m["err"] == "42704", m["err"])
        c.close()
    finally:
        stop_server(proc)
    print(f"protocol_test83: {passed} passed, {failed} failed")
    return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(main())
