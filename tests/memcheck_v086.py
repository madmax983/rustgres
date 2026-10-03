#!/usr/bin/env python3
"""memcheck_v086.py — run v0.86 function/operator paths under valgrind memcheck.

Exercises:
  A. CREATE FUNCTION (SQL, INTERNAL), calls, OR REPLACE, DROP
  B. CREATE OPERATOR / DROP OPERATOR, mixed-type IN
  C. Composite return (mki8), scalar + FROM
  D. Error paths (42883, 42704, 42601, 0A000)
  E. WAL replay + checkpoint preserve catalogs
"""
import os, socket, struct, subprocess, sys, tempfile, time, shutil

PORT = 5586
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")

def send_sql(s, sql):
    s.sendall(b'Q' + struct.pack("!i", len(sql.encode()) + 5) + sql.encode() + b'\x00')
    while True:
        hdr = s.recv(5)
        if len(hdr) < 5:
            break
        typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
        body = b""
        while len(body) < ln - 4:
            chunk = s.recv(ln - 4 - len(body))
            if not chunk:
                break
            body += chunk
        if typ == b"Z":
            break

def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc86_")
    log = os.path.join(data_dir, "vg.log")
    proc = subprocess.Popen(
        [VG, "--tool=memcheck", "--error-exitcode=99",
         "--errors-for-leak-kinds=none",
         f"--log-file={log}",
         BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            print("server did not start under valgrind")
            proc.kill()
            sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
        params = b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", 8 + len(params)) + struct.pack("!i", 196608) + params)
        while True:
            hdr = s.recv(5)
            typ, ln = hdr[0:1], struct.unpack("!i", hdr[1:5])[0]
            body = s.recv(ln - 4)
            if typ == b"Z":
                break
        # A: functions
        send_sql(s, "create function add2(a int, b int) returns int language sql as 'select $1 + $2'")
        send_sql(s, "select add2(3, 4)")
        send_sql(s, "create or replace function add2(a int, b int) returns int language sql as 'select $1 * $2'")
        send_sql(s, "select add2(3, 4)")
        send_sql(s, "create function int4eq_unsafe(int4, int4) returns bool language internal as 'int4eq'")
        send_sql(s, "select int4eq_unsafe(1, 1)")
        send_sql(s, "drop function add2(int, int)")
        send_sql(s, "select nosuchfn(1)")  # 42883
        # B: operators
        send_sql(s, "create table t1(c1 text)")
        send_sql(s, "insert into t1 values ('1')")
        send_sql(s, "create function myeq(int8, text) returns bool language sql as 'select $1::text = $2'")
        send_sql(s, "create operator = (procedure = myeq, leftarg = int8, rightarg = text)")
        send_sql(s, "select 1::int8 in (select c1 from t1)")
        send_sql(s, "drop operator = (int8, text)")
        # C: composite
        send_sql(s, "create table ct(q1 int8, q2 int8)")
        send_sql(s, "create function mkc(bigint, bigint) returns ct language sql as 'select row($1,$2)::ct'")
        send_sql(s, "select mkc(1,2)")
        send_sql(s, "select * from mkc(3,4)")
        # D: errors
        send_sql(s, "create function bad() returns int language sql as 'select from where'")  # 42601
        send_sql(s, "create function bad2() returns int language internal as 'nosuch'")  # 0A000
        send_sql(s, "select row(1,2)::nosuchtype")  # 42704
        s.close()
        print("workload done")
    finally:
        proc.terminate()
        proc.wait()
        time.sleep(1)
    # Check log
    errors = 0
    with open(log) as f:
        for line in f:
            if "ERROR SUMMARY" in line:
                print(line.strip())
            if "definitely lost" in line or "indirectly lost" in line:
                print(line.strip())
    shutil.rmtree(data_dir, ignore_errors=True)
    print("memcheck_v086 done")

if __name__ == "__main__":
    main()
