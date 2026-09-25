#!/usr/bin/env python3
"""memcheck_v090.py — run the v0.90 text-operator / variadic / datestyle
paths under valgrind memcheck.

Exercises every new v0.90 path:
  A. `||` operator: text||int (ok), int||numeric -> 42883 (new error path)
  B. concat/concat_ws with dates under PGDATESTYLE=Postgres, MDY
     (new datestyle branch in format_date)
  C. VARIADIC: NULL array -> NULL, empty array -> '', element expansion
  D. Knuth div_rem hot path via numeric division (regression-guard the
     v0.90 BigUint rewrite under memcheck)

The server is started with PGDATESTYLE=Postgres, MDY to cover the new
date-format branch.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5590
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def cstr(s):
    return s.encode() + b"\x00"


def msg(t, payload):
    return t + struct.pack("!I", 4 + len(payload)) + payload


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


class Conn:
    def __init__(self):
        self.s = socket.create_connection(("127.0.0.1", PORT), timeout=15)
        params = b"user\x00postgres\x00database\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!I", 8 + len(params)) + b"\x00\x03\x00\x00" + params)
        while True:
            t, p = read_msg(self.s)
            if t == b"Z":
                break

    def q(self, sql):
        self.s.sendall(msg(b"Q", cstr(sql)))
        rows = []
        while True:
            t, p = read_msg(self.s)
            if t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos:pos+4])
                    pos += 4
                    if ln == -1:
                        r.append(None)
                    else:
                        r.append(p[pos:pos+ln].decode(errors="replace"))
                        pos += ln
                rows.append(r)
            elif t in (b"E", b"Z"):
                if t == b"Z":
                    break
        return rows

    def close(self):
        self.s.close()


QUERIES = [
    # A. || operator (new 42883 path + normal path)
    "select 'four: ' || 2+2;",
    "select 3 || 4.0;",  # -> 42883
    "select 'a' || 'b' || 'c';",
    # B. concat/concat_ws with dates (PGDATESTYLE branch)
    "select concat(1,2,3,'hello',true, false, to_date('20100309','YYYYMMDD'));",
    "select concat_ws('#',1,2,3,'hello',true, false, to_date('20100309','YYYYMMDD'));",
    "select to_date('20100309','YYYYMMDD');",  # bare date output
    # C. VARIADIC expansion
    "select concat_ws(',', variadic NULL::int[]);",
    "select concat(variadic NULL::int[]) is NULL;",
    "select concat(variadic '{}'::int[]) = '';",
    "select concat(variadic '{1,2,3}'::int[]);",
    # D. Knuth div_rem via numeric (v0.90 rewrite)
    "select 1/3::numeric;",
    "select sqrt(2::numeric);",
    "select ln(2::numeric);",
    "select exp(1::numeric);",
    "select power(10, ln(abs(round(7799461.4119::numeric, 200))));",
]


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc90_")
    log = os.path.join(data_dir, "vg.log")
    env = dict(os.environ, PGDATESTYLE="Postgres, MDY")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("server did not start")
            return 1
        c = Conn()
        for sql in QUERIES:
            try:
                c.q(sql)
            except Exception as e:
                print(f"query failed (continuing): {sql[:60]!r}: {e}")
        c.close()
        time.sleep(1)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
    # Valgrind exit code 99 => memcheck errors found
    rc = proc.returncode
    with open(log) as f:
        log_text = f.read()
    err_count = log_text.count("ERROR SUMMARY")
    print(f"valgrind exit={rc} log={log}")
    if rc == 99:
        print("MEMCHECK FAILURES DETECTED")
        # Print the error summaries
        for line in log_text.splitlines():
            if "ERROR SUMMARY" in line or "definitely lost" in line:
                print(line)
        return 1
    print("memcheck clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
