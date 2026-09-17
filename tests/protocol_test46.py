#!/usr/bin/env python3
"""Protocol 46: set-operation ORDER BY validation on empty results.

v0.46 fixes a genuine v0.44/v0.45 bug: set-operation ORDER BY terms were
only validated per result row, so `... EXCEPT ... ORDER BY q2` (q2 not an
output column) silently succeeded whenever the combined result was empty.
PG19 resolves set-op ORDER BY at analysis time against the leftmost
branch's output names (analyze.c: transformSetOperationStmt builds the
"dummy vars and their names for use in parsing ORDER BY"), so the error
(42703) must fire regardless of row count.

RED on v0.45: EXCEPT ... ORDER BY <unknown> -> success (no error).
GREEN on v0.46: 42703 (unknown name), 42803 (bad ordinal), and valid
terms sort correctly on empty and non-empty results.
"""
import socket, struct, subprocess, time, os

PORT = 5546
DATA_DIR = "/tmp/rg46proto"
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "rustgres")


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d


def connect():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"Z":
            break
        if t == b"E":
            raise RuntimeError(f"startup failed: {b!r}")
    return s


def run_sql(s, sql):
    s.sendall(b"Q" + struct.pack("!i", len(sql.encode()) + 5) + sql.encode() + b"\x00")
    rows = []
    err = None
    errcode = None
    while True:
        t = read_exact(s, 1)
        ln = struct.unpack("!i", read_exact(s, 4))[0]
        b = read_exact(s, ln - 4)
        if t == b"D":
            (n,) = struct.unpack("!h", b[:2])
            pos = 2
            row = []
            for _ in range(n):
                (ln2,) = struct.unpack("!i", b[pos:pos + 4])
                pos += 4
                if ln2 == -1:
                    row.append(None)
                else:
                    row.append(b[pos:pos + ln2].decode())
                    pos += ln2
            rows.append(row)
        elif t == b"E":
            i = 0
            msg = ""
            code = ""
            while i < len(b) and b[i] != 0:
                f = chr(b[i])
                i += 1
                j = b.find(b"\x00", i)
                v = b[i:j].decode(errors="replace")
                i = j + 1
                if f == "M":
                    msg = v
                elif f == "C":
                    code = v
            err = msg
            errcode = code
        elif t == b"Z":
            break
    return rows, err, errcode


def main():
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg46proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        # (sql, expected_rows_or_None, expected_errcode_or_None, description)
        cases = [
            # --- The bug: EXCEPT with empty result must still validate ORDER BY
            ("SELECT 1 AS q1 EXCEPT SELECT 1 ORDER BY q2", None, "42703",
             "EXCEPT empty result, unknown ORDER BY name"),
            ("SELECT 1 AS q1 EXCEPT ALL SELECT 1 ORDER BY q2", None, "42703",
             "EXCEPT ALL empty result, unknown ORDER BY name"),
            ("SELECT 1 AS q1 EXCEPT SELECT 1 ORDER BY 2", None, "42803",
             "EXCEPT empty result, ORDER BY ordinal out of range"),
            ("SELECT 1 AS q1 EXCEPT SELECT 1 ORDER BY q1 + 1", None, "42703",
             "EXCEPT empty result, ORDER BY expression"),
            # Same bug shape on UNION/INTERSECT with empty results (already
            # green before the fix; must stay green)
            ("SELECT 1 AS q1 INTERSECT SELECT 2 ORDER BY q2", None, "42703",
             "INTERSECT empty result, unknown ORDER BY name"),
            ("SELECT 1 AS q1 UNION SELECT 2 ORDER BY q2 LIMIT 0", None, "42703",
             "UNION empty-via-limit result, unknown ORDER BY name"),
            # --- Valid ORDER BY terms still work, empty and non-empty
            ("SELECT 1 AS q1 EXCEPT SELECT 1 ORDER BY q1", [], None,
             "EXCEPT empty result, valid ORDER BY name"),
            ("SELECT 1 AS q1 EXCEPT SELECT 1 ORDER BY 1", [], None,
             "EXCEPT empty result, valid ORDER BY ordinal"),
            ("SELECT 2 AS q1 EXCEPT SELECT 1 AS q1 ORDER BY q1",
             [["2"]], None, "EXCEPT non-empty, valid ORDER BY name"),
            ("SELECT 3 AS a UNION SELECT 1 AS a ORDER BY a DESC",
             [["3"], ["1"]], None, "UNION valid ORDER BY name DESC"),
            ("SELECT 3 AS a INTERSECT SELECT 3 ORDER BY 1",
             [["3"]], None, "INTERSECT valid ORDER BY ordinal"),
            ("SELECT 3 AS a UNION SELECT 1 ORDER BY 1 LIMIT 1 OFFSET 1",
             [["3"]], None, "UNION ORDER BY + OFFSET + LIMIT"),
            # Chained set-ops with root ORDER BY
            ("SELECT 1 AS x UNION SELECT 2 EXCEPT SELECT 3 ORDER BY x",
             [["1"], ["2"]], None, "chained UNION/EXCEPT ORDER BY"),
            ("SELECT 1 AS x UNION SELECT 2 EXCEPT SELECT 3 ORDER BY nosuch",
             None, "42703", "chained UNION/EXCEPT unknown ORDER BY name"),
        ]

        for sql, exp_rows, exp_code, desc in cases:
            rows, err, code = run_sql(s, sql)
            if exp_code:
                if code == exp_code:
                    print(f"PASS: {desc} (got {code})")
                    passed += 1
                else:
                    print(f"FAIL: {desc}: expected {exp_code}, got {code} ({err})")
                    failed += 1
            else:
                if rows == exp_rows and code is None:
                    print(f"PASS: {desc}")
                    passed += 1
                else:
                    print(f"FAIL: {desc}: expected rows {exp_rows}, got {rows} code={code} ({err})")
                    failed += 1

        print(f"\n{passed} passed, {failed} failed")
        return 1 if failed else 0
    finally:
        proc.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
