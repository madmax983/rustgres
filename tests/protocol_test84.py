#!/usr/bin/env python3
"""protocol_test84: v0.83 extended-protocol Bind robustness.

A. Zero-parameter Bind (regression net for the v0.82 "message truncated"
   report — root-caused to a malformed client message, not the 0-param path):
   uncached and parse-cache-hit zero-param Bind, named portals, empty query,
   all-NULL params, fewer/more params than required.
B. v0.83 hardening: structurally truncated Parse/Bind/Describe/Execute/Close
   payloads now get 08P01 with the connection surviving (PG's
   pq_getmsgint raises ERRCODE_PROTOCOL_VIOLATION the same way); previously
   the io "message truncated" error killed the connection.
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

def msg(t, body):
    return t + struct.pack("!i", 4 + len(body)) + body

def parse_msg(t, sql, oids=()):
    return msg(b"P", t.encode() + b"\x00" + sql.encode() + b"\x00"
               + struct.pack("!h", len(oids)) + b"".join(struct.pack("!i", o) for o in oids))

def bind_msg(portal, stmt, pformats=(), params=(), rformats=()):
    bb = portal.encode() + b"\x00" + stmt.encode() + b"\x00"
    bb += struct.pack("!h", len(pformats)) + b"".join(struct.pack("!h", f) for f in pformats)
    bb += struct.pack("!h", len(params))
    for p in params:
        if p is None: bb += struct.pack("!i", -1)
        else:
            pb = p.encode(); bb += struct.pack("!i", len(pb)) + pb
    bb += struct.pack("!h", len(rformats)) + b"".join(struct.pack("!h", f) for f in rformats)
    return msg(b"B", bb)

def exec_msg(portal, max_rows=0):
    return msg(b"E", portal.encode() + b"\x00" + struct.pack("!i", max_rows))

class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
        self.drain()
    def drain(self):
        """Read until ReadyForQuery; return list of (type, body)."""
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
    def send(self, *msgs):
        self.s.sendall(b"".join(msgs) + msg(b"S", b""))
        return self.drain()
    def close(self):
        self.s.sendall(msg(b"X", b""))
        self.s.close()

def errcode(msgs):
    for typ, body in msgs:
        if typ == b"E":
            pos = 0
            while pos < len(body) - 1:
                f, e = body[pos:pos+1], body.find(b"\x00", pos + 1)
                if e < 0: break
                if f == b"C": return body[pos+1:e].decode()
                pos = e + 1
    return None

def types(msgs):
    return b"".join(t for t, _ in msgs)

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
    proc.terminate()
    raise RuntimeError("server did not start")

def main():
    data_dir = tempfile.mkdtemp(prefix="rg84_")
    proc = start_server(data_dir)
    try:
        # ---------- A. zero-parameter Bind ----------
        c = Conn()
        # A1: uncached zero-param Bind, full cycle
        m = c.send(parse_msg("a1", "SELECT 1"), bind_msg("", "a1"), exec_msg(""))
        check("A1 uncached 0-param Bind", types(m) == b"12DCZ" and datarows(m) == [["1"]],
              (types(m), errcode(m), datarows(m)))
        # A2: cached zero-param Bind (same SQL -> parse-cache hit)
        m = c.send(parse_msg("a2", "SELECT 1"), bind_msg("", "a2"), exec_msg(""))
        check("A2 cached 0-param Bind", types(m) == b"12DCZ" and datarows(m) == [["1"]],
              (types(m), errcode(m)))
        # A3: named portal, zero params
        m = c.send(parse_msg("a3", "SELECT 42"), bind_msg("port9", "a3"), exec_msg("port9"))
        check("A3 named portal 0-param", types(m) == b"12DCZ" and datarows(m) == [["42"]],
              (types(m), errcode(m)))
        # A4: empty prepared query -> EmptyQueryResponse (no CommandComplete)
        m = c.send(parse_msg("a4", ""), bind_msg("", "a4"), exec_msg(""))
        check("A4 empty query Bind", types(m) == b"12IZ" and errcode(m) is None,
              (types(m), errcode(m)))
        # A5: all-NULL params
        m = c.send(parse_msg("a5", "SELECT $1::int, $2::text"),
                   bind_msg("", "a5", params=(None, None)), exec_msg(""))
        check("A5 all-NULL params", types(m) == b"12DCZ" and datarows(m) == [[None, None]],
              (types(m), errcode(m), datarows(m)))
        # A6: fewer params than required -> 08P01, connection survives
        m = c.send(parse_msg("a6", "SELECT $1::int, $2::int"),
                   bind_msg("", "a6", params=("7",)), exec_msg(""))
        check("A6 fewer params -> 08P01", errcode(m) == "08P01", (types(m), errcode(m)))
        m = c.send(parse_msg("a6b", "SELECT 9"), bind_msg("", "a6b"), exec_msg(""))
        check("A6b conn alive after 08P01", types(m) == b"12DCZ" and datarows(m) == [["9"]],
              (types(m), errcode(m)))
        # A7: more params than required -> 08P01
        m = c.send(parse_msg("a7", "SELECT $1::int"),
                   bind_msg("", "a7", params=("1", "2")), exec_msg(""))
        check("A7 more params -> 08P01", errcode(m) == "08P01", (types(m), errcode(m)))
        # A8: one format code with zero params is legal (PG: a single code
        # applies to all parameters, vacuously); two codes mismatching the
        # count is 08P01.
        m = c.send(parse_msg("a8", "SELECT 1"),
                   msg(b"B", b"\x00a8\x00" + struct.pack("!h", 1) + struct.pack("!h", 0)
                       + struct.pack("!h", 0) + struct.pack("!h", 0)))
        check("A8 nformats=1 nparams=0 ok", types(m) == b"12Z" and errcode(m) is None,
              (types(m), errcode(m)))
        m = c.send(parse_msg("a8b", "SELECT 1"),
                   msg(b"B", b"\x00a8b\x00" + struct.pack("!h", 2)
                       + struct.pack("!h", 0) + struct.pack("!h", 0)
                       + struct.pack("!h", 0) + struct.pack("!h", 0)))
        check("A8b nformats=2 nparams=0 -> 08P01", errcode(m) == "08P01",
              (types(m), errcode(m)))
        # A9: zero-param Bind on a multi-row result
        m = c.send(parse_msg("a9", "SELECT generate_series(1,50) g"),
                   bind_msg("", "a9"), exec_msg(""))
        check("A9 0-param 50 rows",
              types(m).startswith(b"12D") and types(m).endswith(b"CZ") and len(datarows(m)) == 50,
              (types(m)[:8], len(datarows(m)), errcode(m)))
        c.close()

        # ---------- B. truncated-message hardening ----------
        c = Conn()
        # B1: truncated Bind (claims 1 param, supplies none) -> 08P01, alive
        trunc_bind = msg(b"B", b"\x00" + b"s1\x00" + struct.pack("!h", 0) + struct.pack("!h", 1))
        m = c.send(parse_msg("s1", "SELECT 1"), trunc_bind)
        check("B1 truncated Bind -> 08P01", types(m) == b"1EZ" and errcode(m) == "08P01",
              (types(m), errcode(m)))
        m = c.send(parse_msg("s1b", "SELECT 2"), bind_msg("", "s1b"), exec_msg(""))
        check("B1b conn usable after", types(m) == b"12DCZ" and datarows(m) == [["2"]],
              (types(m), errcode(m)))
        # B2: truncated Parse (cut mid-query cstring) -> 08P01, alive
        m = c.send(msg(b"P", b"\x00\x00"))
        check("B2 truncated Parse -> 08P01", types(m) == b"EZ" and errcode(m) == "08P01",
              (types(m), errcode(m)))
        # B3: Bind with negative param count -> 08P01
        m = c.send(parse_msg("s3", "SELECT 1"),
                   msg(b"B", b"\x00s3\x00" + struct.pack("!h", 0) + struct.pack("!h", -1)))
        check("B3 negative nparams -> 08P01", errcode(m) == "08P01", (types(m), errcode(m)))
        # B4: unterminated Close cstring -> 08P01, alive
        m = c.send(msg(b"C", b"S"))
        check("B4 truncated Close -> 08P01", types(m) == b"EZ" and errcode(m) == "08P01",
              (types(m), errcode(m)))
        # B5: invalid Close kind -> 08P01
        m = c.send(msg(b"C", b"X\x00"))
        check("B5 bad Close kind -> 08P01", types(m) == b"EZ" and errcode(m) == "08P01",
              (types(m), errcode(m)))
        # B6: truncated Describe -> 08P01, alive
        m = c.send(msg(b"D", b"S"))
        check("B6 truncated Describe -> 08P01", types(m) == b"EZ" and errcode(m) == "08P01",
              (types(m), errcode(m)))
        # B7: truncated Execute -> 08P01, alive
        m = c.send(msg(b"E", b""))
        check("B7 truncated Execute -> 08P01", types(m) == b"EZ" and errcode(m) == "08P01",
              (types(m), errcode(m)))
        # B8: final liveness proof
        m = c.send(parse_msg("s8", "SELECT 8"), bind_msg("", "s8"), exec_msg(""))
        check("B8 conn alive at end", types(m) == b"12DCZ" and datarows(m) == [["8"]],
              (types(m), errcode(m)))
        c.close()
    finally:
        proc.terminate(); proc.wait()

    print(f"protocol_test84: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
