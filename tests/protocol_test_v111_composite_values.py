#!/usr/bin/env python3
r"""v1.11 protocol tests: composite value expressions (ARRAY[...] and ROW(...)).

Covers (simple protocol):
- UNION / INTERSECT / EXCEPT over VALUES with array[...] columns
  (dedup and set semantics on integer arrays)
- UNION / INTERSECT / EXCEPT over VALUES with row(...) columns
- row-valued subquery in a row comparison: ROW(1,2) = (SELECT f1, f2)
  (correlated and uncorrelated), subquery on either side
- scalar-context multi-column subquery still raises 42601
- multi-row row-subquery raises 21000
- ARRAY[...] subscript on a subquery result
- = ANY (array[...]) in a join predicate

Extended protocol:
- Parse/Describe of a UNION over array columns reports _int4 (1007),
  not text (25), proving Describe/execution agree.
- Parse/Describe of a UNION over row columns reports record (2249).

Self-starting: launches rustgres on 5591 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil, re

PORT = 5591
SRC_BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")
BIN = "/tmp/rg-v111-proto-bin"
DATADIR = "/tmp/rg_proto_v111_composite"


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


def connect():
    for _ in range(30):
        try:
            s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
            break
        except ConnectionRefusedError:
            time.sleep(0.5)
    else:
        raise RuntimeError("could not connect")
    body = struct.pack("!I", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!I", len(body) + 4) + body)
    while True:
        t, p = read_msg(s)
        if t == b"Z":
            break
    return s


def q(s, sql):
    """Run simple-query SQL, return (rows, error_code)."""
    s.sendall(msg(b"Q", cstr(sql)))
    rows, err = [], None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            pass
        elif t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            j, vals = 2, []
            for _ in range(n):
                ln = struct.unpack("!i", p[j:j+4])[0]; j += 4
                if ln == -1:
                    vals.append(None)
                else:
                    vals.append(p[j:j+ln].decode()); j += ln
            rows.append(tuple(vals))
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return rows, err


def parse_rowdesc(payload):
    """Parse a RowDescription (T) payload into [(name, type_oid)]."""
    n = struct.unpack("!h", payload[:2])[0]
    j, out = 2, []
    for _ in range(n):
        end = payload.index(b"\x00", j)
        name = payload[j:end].decode()
        j = end + 1
        table_oid, col_no = struct.unpack("!ih", payload[j:j+6]); j += 6
        type_oid, = struct.unpack("!i", payload[j:j+4]); j += 4
        j += 2 + 4 + 2  # typlen, typmod, format
        out.append((name, type_oid))
    return out


def extended_describe_oids(s, sql):
    """Parse/Describe via extended protocol; return [(name, type_oid)] or (None, err)."""
    s.sendall(msg(b"P", cstr("") + cstr(sql) + struct.pack("!h", 0)))
    s.sendall(msg(b"D", b"S" + cstr("")))
    s.sendall(msg(b"S", b""))
    desc, err = None, None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            desc = parse_rowdesc(p)
        elif t == b"E":
            m = re.search(rb"C([0-9A-Z]{5})", p)
            err = m.group(1).decode() if m else "?????"
        elif t == b"Z":
            break
    return desc, err


def main():
    shutil.copy2(SRC_BIN, BIN)
    os.chmod(BIN, 0o755)
    shutil.rmtree(DATADIR, ignore_errors=True)
    os.makedirs(DATADIR)
    srv = subprocess.Popen([BIN, "--data-dir", DATADIR, "--port", str(PORT)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    fails = []
    def check(name, cond, detail=""):
        print(("PASS " if cond else "FAIL ") + name + ((" | " + str(detail)) if detail and not cond else ""))
        if not cond:
            fails.append(name)

    try:
        s = connect()

        # --- array set operations ---
        rows, err = q(s, "select x from (values (array[1, 2]), (array[1, 3])) _(x) "
                         "union select x from (values (array[1, 2]), (array[1, 4])) _(x)")
        check("array union dedup", err is None and sorted(r[0] for r in rows) == ["{1,2}", "{1,3}", "{1,4}"], (rows, err))
        rows, err = q(s, "select x from (values (array[1, 2]), (array[1, 3])) _(x) "
                         "intersect select x from (values (array[1, 2]), (array[1, 4])) _(x)")
        check("array intersect", err is None and rows == [("{1,2}",)], (rows, err))
        rows, err = q(s, "select x from (values (array[1, 2]), (array[1, 3])) _(x) "
                         "except select x from (values (array[1, 2]), (array[1, 4])) _(x)")
        check("array except", err is None and rows == [("{1,3}",)], (rows, err))

        # --- record set operations ---
        rows, err = q(s, "select x from (values (row(1, 2)), (row(1, 3))) _(x) "
                         "union select x from (values (row(1, 2)), (row(1, 4))) _(x)")
        check("record union dedup", err is None and sorted(r[0] for r in rows) == ["(1,2)", "(1,3)", "(1,4)"], (rows, err))
        rows, err = q(s, "select x from (values (row(1, 2)), (row(1, 3))) _(x) "
                         "intersect select x from (values (row(1, 2)), (row(1, 4))) _(x)")
        check("record intersect", err is None and rows == [("(1,2)",)], (rows, err))
        rows, err = q(s, "select x from (values (row(1, 2)), (row(1, 3))) _(x) "
                         "except select x from (values (row(1, 2)), (row(1, 4))) _(x)")
        check("record except", err is None and rows == [("(1,3)",)], (rows, err))

        # --- row-valued subqueries ---
        q(s, "CREATE TABLE subselect_tbl (f1 int, f2 int)")
        q(s, "INSERT INTO subselect_tbl VALUES (1, 2), (3, 4)")
        rows, err = q(s, "SELECT ROW(1, 2) = (SELECT f1, f2) AS eq FROM SUBSELECT_TBL")
        check("row = correlated row-subquery", err is None and rows == [("t",), ("f",)], (rows, err))
        rows, err = q(s, "SELECT ROW(1, 2) = (SELECT 3, 4) AS eq FROM SUBSELECT_TBL")
        check("row = uncorrelated row-subquery", err is None and rows == [("f",), ("f",)], (rows, err))
        rows, err = q(s, "SELECT (SELECT f1, f2 FROM SUBSELECT_TBL LIMIT 1) = ROW(1, 2)")
        check("row-subquery on left", err is None and rows == [("t",)], (rows, err))
        rows, err = q(s, "SELECT ROW(1, 2) = (SELECT f1, f2 FROM SUBSELECT_TBL)")
        check("multi-row row-subquery is 21000", err == "21000", (rows, err))
        rows, err = q(s, "SELECT (SELECT 1, 2)")
        check("scalar multi-column subquery still 42601", err == "42601", (rows, err))

        # --- array subscripts and ANY ---
        rows, err = q(s, "SELECT (SELECT ARRAY[1,2,3])[1]")
        check("subscript on subquery array", err is None and rows == [("1",)], (rows, err))
        rows, err = q(s, "select * from (values (1, array[10,20]), (2, array[20,30])) as v1(v1x,v1ys) "
                         "where v1x = any (array[1,2])")
        check("= ANY (array[...])", err is None and len(rows) == 2, (rows, err))

        # --- extended protocol Describe OIDs ---
        desc, err = extended_describe_oids(s, "select x from (values (array[1, 2]), (array[1, 3])) _(x) "
                                              "union select x from (values (array[1, 2])) _(x)")
        check("describe array union is _int4", err is None and desc == [("x", 1007)], (desc, err))
        desc, err = extended_describe_oids(s, "select x from (values (row(1, 2))) _(x) "
                                              "union select x from (values (row(3, 4))) _(x)")
        check("describe record union is record", err is None and desc == [("x", 2249)], (desc, err))

        s.close()
    finally:
        srv.terminate()
        srv.wait()
    print("v1.11 protocol: %d failures" % len(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
