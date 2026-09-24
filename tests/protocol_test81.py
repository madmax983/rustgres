#!/usr/bin/env python3
"""protocol_test81.py — v0.79 protocol coverage (RED against exact v0.78 base).

v0.79 changes under test: a real array type.
A. Array DDL: int[]/text[]/int[][] columns; INSERT of '{...}' literals;
   SELECT renders PG array_out text; RowDescription carries 1007/1009.
B. ARRAY[...] constructor: typed arrays, common-type coercion, pg_typeof.
C. array(SELECT ...) produces real typed array values (not text).
D. Subscripting: a[i], a[i][j]; PG semantics (1-based, OOB -> NULL).
E. Slices: a[l:u] with PG bound semantics.
F. array_length / cardinality / array_dims / array_ndims /
   array_upper / array_lower.
G. unnest(...) as a FROM-clause table function and scalar SRF.
H. Casts: text->int[], int[]->text, int[]->bigint[] (element-wise).
I. = / <> on arrays (PG three-valued element-wise semantics).
J. || concatenation: array||array, array||elem, elem||array.

This test manages its own server on port 5553.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

PORT = 5553
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

passed = 0
failed = 0


def check(name, cond, extra=""):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name} {extra}")


def read_msg(s):
    typ = s.recv(1)
    ln = struct.unpack("!i", s.recv(4))[0]
    payload = b""
    while len(payload) < ln - 4:
        chunk = s.recv(ln - 4 - len(payload))
        if not chunk:
            raise RuntimeError("connection closed")
        payload += chunk
    return typ, payload


def parse_fields(payload):
    out = {}
    pos = 0
    while pos < len(payload) - 1:
        kind = payload[pos:pos + 1]
        end = payload.index(b"\x00", pos + 1)
        out[kind] = payload[pos + 1:end].decode(errors="replace")
        pos = end + 1
    return out


def err_code(msgs):
    for t, p in msgs:
        if t == b"E":
            return parse_fields(p).get(b"C")
    return None


def rowdesc_oids(msgs):
    for t, p in msgs:
        if t == b"T":
            oids = []
            n = struct.unpack("!h", p[:2])[0]
            pos = 2
            for _ in range(n):
                end = p.index(b"\x00", pos)
                pos = end + 1
                pos += 4 + 2
                (oid,) = struct.unpack("!i", p[pos:pos + 4])
                oids.append(oid)
                pos += 4 + 2 + 4 + 2
            return oids
    return None


def parse_datarow(payload):
    out = []
    nfields = struct.unpack("!h", payload[:2])[0]
    pos = 2
    for _ in range(nfields):
        ln = struct.unpack("!i", payload[pos:pos + 4])[0]
        pos += 4
        if ln == -1:
            out.append(None)
        else:
            out.append(payload[pos:pos + ln].decode())
            pos += ln
    return out


def rows_of(msgs):
    return [parse_datarow(p) for t, p in msgs if t == b"D"]


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = read_msg(self.s)
            if t == b"Z":
                break

    def query(self, sql):
        self.s.sendall(b"Q" + struct.pack("!i", len(sql.encode()) + 5)
                       + sql.encode() + b"\x00")
        msgs = []
        while True:
            t, p = read_msg(self.s)
            msgs.append((t, p))
            if t == b"Z":
                return msgs

    def close(self):
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
    data_dir = tempfile.mkdtemp(prefix="rg81_")
    proc = start_server(data_dir)
    try:
        c = Conn()

        # ---- A. Array DDL + storage ----
        m = c.query("CREATE TABLE arr1 (a int, b int[], c text[])")
        check("A1 create table with int[]/text[]", err_code(m) is None, err_code(m))
        m = c.query("INSERT INTO arr1 VALUES (1, '{2,3}', '{\"a\",\"b\"}'), (2, '{}', NULL)")
        check("A2 insert array literals", err_code(m) is None, err_code(m))
        m = c.query("SELECT b, c FROM arr1 ORDER BY a")
        check("A3 select renders array_out text", err_code(m) is None and rows_of(m) == [["{2,3}", "{a,b}"], ["{}", None]], (err_code(m), rows_of(m)))
        check("A4 rowdesc OIDs 1007/1009", rowdesc_oids(m) == [1007, 1009], rowdesc_oids(m))
        m = c.query("CREATE TABLE arr2 (m int[3], n int[][])")
        check("A5 sized/multi-dim DDL accepted", err_code(m) is None, err_code(m))

        # ---- B. ARRAY[...] constructor ----
        m = c.query("SELECT ARRAY[1,2,3]")
        check("B1 array ctor", err_code(m) is None and rows_of(m) == [["{1,2,3}"]], (err_code(m), rows_of(m)))
        check("B2 array ctor OID 1007", rowdesc_oids(m) == [1007], rowdesc_oids(m))
        m = c.query("SELECT pg_typeof(ARRAY[1,2]), pg_typeof(ARRAY['a','b'])")
        check("B3 pg_typeof integer[]/text[]", err_code(m) is None and rows_of(m) == [["integer[]", "text[]"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT ARRAY[1, 2.5]")
        check("B4 ctor common-type coercion to numeric[]", err_code(m) is None and rows_of(m) == [["{1,2.5}"]], (err_code(m), rows_of(m)))
        check("B5 ctor numeric OID 1231", rowdesc_oids(m) == [1231], rowdesc_oids(m))
        m = c.query("SELECT ARRAY[]")
        check("B6 empty ctor is 42P08", err_code(m) == "42P08", err_code(m))

        # ---- C. array(subquery) ----
        m = c.query("SELECT array(SELECT x FROM (VALUES (1),(2)) v(x))")
        check("C1 array(subquery) value", err_code(m) is None and rows_of(m) == [["{1,2}"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT pg_typeof(array(SELECT x FROM (VALUES (1),(2)) v(x)))")
        check("C2 array(subquery) pg_typeof integer[]", err_code(m) is None and rows_of(m) == [["integer[]"]], (err_code(m), rows_of(m)))

        # ---- D. Subscripting ----
        m = c.query("SELECT (ARRAY[10,20,30])[2], (ARRAY[10,20,30])[0], (ARRAY[10,20,30])[5]")
        check("D1 subscript 1-based, OOB -> NULL", err_code(m) is None and rows_of(m) == [["20", None, None]], (err_code(m), rows_of(m)))
        m = c.query("SELECT ('{{1,2},{3,4}}'::int[])[2][1]")
        check("D2 multi-dim subscript", err_code(m) is None and rows_of(m) == [["3"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT ('{{1,2},{3,4}}'::int[])[2]")
        check("D2b partial subscript -> NULL (PG19 array_get_element)", err_code(m) is None and rows_of(m) == [[None]], (err_code(m), rows_of(m)))
        m = c.query("SELECT ('{1}'::int[])[1][1][1][1][1][1][1]")
        check("D2c >6 subscripts is 54000", err_code(m) == "54000", err_code(m))
        m = c.query("SELECT (NULL::int[])[1]")
        check("D3 null array subscript -> NULL", err_code(m) is None and rows_of(m) == [[None]], (err_code(m), rows_of(m)))
        m = c.query("SELECT 5[1]")
        check("D4 subscript of non-array errors", err_code(m) is not None, err_code(m))

        # ---- E. Slices ----
        m = c.query("SELECT ('{1,2,3,4}'::int[])[2:3]")
        check("E1 slice", err_code(m) is None and rows_of(m) == [["{2,3}"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT array_dims(('{1,2,3,4}'::int[])[2:3])")
        check("E2 slice resets lower bound to 1 (PG19 array_get_slice)", err_code(m) is None and rows_of(m) == [["[1:2]"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT ('{{1,2,3},{4,5,6}}'::int[])[1:2][2:3]")
        check("E3 multi-dim slice chain (plain [i] -> [1:i])", err_code(m) is None and rows_of(m) == [["{{2,3},{5,6}}"]], (err_code(m), rows_of(m)))

        # ---- F. array_* functions ----
        m = c.query("SELECT array_length(ARRAY[1,2,3], 1), array_length(ARRAY[1,2,3], 2), cardinality(ARRAY[1,2,3])")
        check("F1 array_length/cardinality", err_code(m) is None and rows_of(m) == [["3", None, "3"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT array_ndims('{{1,2},{3,4}}'::int[]), array_dims('{{1,2},{3,4}}'::int[])")
        check("F2 ndims/dims", err_code(m) is None and rows_of(m) == [["2", "[1:2][1:2]"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT array_lower(('{5,6,7}'::int[])[2:3], 1), array_upper(('{5,6,7}'::int[])[2:3], 1)")
        check("F3 lower/upper of slice (bounds reset to 1)", err_code(m) is None and rows_of(m) == [["1", "2"]], (err_code(m), rows_of(m)))

        # ---- G. unnest ----
        m = c.query("SELECT * FROM unnest(ARRAY[1,2,3])")
        check("G1 unnest table function", err_code(m) is None and rows_of(m) == [["1"], ["2"], ["3"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT unnest(ARRAY['a','b'])")
        check("G2 unnest scalar SRF", err_code(m) is None and rows_of(m) == [["a"], ["b"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT * FROM unnest(NULL::int[])")
        check("G3 unnest NULL -> zero rows", err_code(m) is None and rows_of(m) == [], (err_code(m), rows_of(m)))

        # ---- H. Casts ----
        m = c.query("SELECT '{1,2,3}'::int[], '{1,NULL,3}'::int[], '{\"a,b\",\"c\"}'::text[]")
        check("H1 text->array casts", err_code(m) is None and rows_of(m) == [["{1,2,3}", "{1,NULL,3}", '{"a,b",c}']], (err_code(m), rows_of(m)))
        m = c.query("SELECT (ARRAY[1,2])::text, (ARRAY[1,2])::bigint[]")
        check("H2 array->text and int[]->bigint[]", err_code(m) is None and rows_of(m) == [["{1,2}", "{1,2}"]], (err_code(m), rows_of(m)))
        check("H3 bigint[] OID 1016", rowdesc_oids(m) == [25, 1016], rowdesc_oids(m))
        m = c.query("SELECT '{a,b}'::int[]")
        check("H4 bad element is 22P02", err_code(m) == "22P02", err_code(m))

        # ---- I. array = / <> ----
        m = c.query("SELECT ARRAY[1,2] = ARRAY[1,2], ARRAY[1,2] = ARRAY[1,3], ARRAY[1,2] = ARRAY[1,2,3]")
        check("I1 array equality", err_code(m) is None and rows_of(m) == [["t", "f", "f"]], (err_code(m), rows_of(m)))
        m = c.query("SELECT ARRAY[1,NULL] = ARRAY[1,NULL], ARRAY[1,2] <> ARRAY[1,3]")
        check("I2 array eq with NULLs -> NULL; <>", err_code(m) is None and rows_of(m) == [[None, "t"]], (err_code(m), rows_of(m)))

        # ---- J. || ----
        m = c.query("SELECT ARRAY[1,2] || ARRAY[3,4], ARRAY[1,2] || 3, 0 || ARRAY[1,2]")
        check("J1 array concat", err_code(m) is None and rows_of(m) == [["{1,2,3,4}", "{1,2,3}", "{0,1,2}"]], (err_code(m), rows_of(m)))

        c.close()
    finally:
        stop_server(proc)
    print(f"protocol_test81: {passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
