#!/usr/bin/env python3
"""Protocol 45: format() per PG19 text_format().

RED on v0.44 (42883: function format() does not exist).
GREEN on v0.45.

Covers: %s/%I/%L/%%, positional args, - flag, widths (direct and *),
NULL format -> NULL, NULL %s -> empty, NULL %L -> 'NULL', NULL %I -> 22004,
bad specifiers -> 22023.
"""
import socket, struct, subprocess, time, os, sys, signal

PORT = 5546
DATA_DIR = "/tmp/rg45proto"
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
                (ln2,) = struct.unpack("!i", b[pos:pos+4])
                pos += 4
                if ln2 == -1:
                    row.append(None)
                else:
                    row.append(b[pos:pos+ln2].decode())
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
    # Start server
    subprocess.run(["rm", "-rf", DATA_DIR], check=False)
    os.makedirs(DATA_DIR, exist_ok=True)
    # Kill any existing
    subprocess.run(["pkill", "-9", "-x", "rustgres"], check=False)
    time.sleep(2)
    log = open("/tmp/rg45proto.log", "w")
    proc = subprocess.Popen([BIN, "--data-dir", DATA_DIR, "--port", str(PORT)],
                            stdout=log, stderr=subprocess.STDOUT)
    time.sleep(4)

    try:
        s = connect()
        passed = 0
        failed = 0

        # (sql, expected_rows_or_None, expected_errcode_or_None, description)
        cases = [
            # Basic %s
            ("SELECT format('Hello %s', 'World');", [["Hello World"]], None, "%s basic"),
            ("SELECT format('Hello');", [["Hello"]], None, "no specifiers"),
            ("SELECT format('Hello %%');", [["Hello %"]], None, "%% escape"),
            # %I and %L
            ("SELECT format('INSERT INTO %I VALUES(%L)', 'mytab', 10);",
             [["INSERT INTO mytab VALUES('10')"]], None, "%I and %L"),
            ("SELECT format('%I', 'my\"tab');", [['"my""tab"']], None, "%I quoting"),
            ("SELECT format('%L', 'O''Brien');", [["'O''Brien'"]], None, "%L quoting"),
            # Positional
            ("SELECT format('%1$s %3$s', 1, 2, 3);", [["1 3"]], None, "positional"),
            ("SELECT format('%2$s %1$s', 'a', 'b');", [["b a"]], None, "positional reorder"),
            # Widths
            ("SELECT format('>>%10s<<', 'Hello');", [["\u003e\u003e     Hello\u003c\u003c"]], None, "width right"),
            ("SELECT format('>>%-10s<<', 'Hello');", [["\u003e\u003eHello     \u003c\u003c"]], None, "width left"),
            ("SELECT format('>>%*s<<', 10, 'Hi');", [["\u003e\u003e        Hi\u003c\u003c"]], None, "width *"),
            ("SELECT format('>>%2$*1$s<<', 10, 'Hi');", [["\u003e\u003e        Hi\u003c\u003c"]], None, "width *n$"),
            # NULL handling
            ("SELECT format(NULL, 'a');", [[None]], None, "NULL format -> NULL"),
            ("SELECT format('%s%s%s', 'a', NULL, 'b');", [["ab"]], None, "NULL %s -> empty"),
            ("SELECT format('%L', NULL);", [["NULL"]], None, "NULL %L -> NULL"),
            ("SELECT format('%I', NULL);", None, "22004", "NULL %I -> 22004"),
            # Errors
            ("SELECT format('Hello %s %s', 'World');", None, "22023", "too few args"),
            ("SELECT format('Hello %x', 20);", None, "22023", "bad specifier"),
            ("SELECT format('%0$s', 'a');", None, "22023", "arg 0"),
            ("SELECT format('Hello %', 'a');", None, "22023", "unterminated"),
            # Bool in format() is true/false (not t/f)
            ("SELECT format('%s', true);", [["true"]], None, "bool true"),
            ("SELECT format('%s', false);", [["false"]], None, "bool false"),
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
                if rows == exp_rows:
                    print(f"PASS: {desc}")
                    passed += 1
                else:
                    print(f"FAIL: {desc}: expected {exp_rows!r}, got {rows!r}")
                    failed += 1

        s.close()
        print(f"\n{passed} passed, {failed} failed")
        return 0 if failed == 0 else 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

if __name__ == "__main__":
    sys.exit(main())
