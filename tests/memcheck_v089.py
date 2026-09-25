#!/usr/bin/env python3
"""memcheck_v089.py — run every new v0.89 path under valgrind memcheck.

Exercises (live server, simple protocol):
  A. USING btree on every CREATE INDEX shape: plain, DESC / NULLS FIRST /
     NULLS LAST, multi-key, UNIQUE, expression, partial, auto-named.
     USING hash (or any non-btree method) must fail with 0A000 and create
     nothing.
  B. Multi-name DROP INDEX atomicity (the v0.88 gate gap): an existing
     name followed by a missing name aborts with 42P01 and leaves the
     existing index in place; missing-first likewise; DROP INDEX IF
     EXISTS skips the missing name and drops the rest.
  C. DML + ORDER BY still correct with USING btree indexes present
     (incl. DESC fast-path fallback to explicit sort).
  D. CHECKPOINT with USING btree metadata present.

Valgrind must report zero errors; every SQL expectation must hold.
"""
import os, socket, struct, subprocess, sys, tempfile, time

PORT = 5591
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "debug", "rustgres")


def main():
    data_dir = tempfile.mkdtemp(prefix="rgmc89_")
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
            print("server did not start under valgrind"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd(n):
            d = b""
            while len(d) < n:
                c = s.recv(n - len(d))
                if not c:
                    raise RuntimeError("closed")
                d += c
            return d

        def msg():
            t = rd(1)
            ln = struct.unpack("!i", rd(4))[0]
            return t, rd(ln - 4)

        while True:
            t, _ = msg()
            if t == b"Z":
                break

        def errcode(p):
            code = None
            i = 0
            while i < len(p) - 1:
                f = p[i:i + 1]
                e = p.find(b"\x00", i + 1)
                if e < 0:
                    break
                if f == b"C":
                    code = p[i + 1:e].decode()
                i = e + 1
            return code

        def simple(q):
            qb = q.encode()
            s.sendall(b"Q" + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
            rows = []
            got_err = None
            while True:
                t, p = msg()
                if t == b"D":
                    n = struct.unpack("!h", p[:2])[0]
                    off = 2
                    r = []
                    for _ in range(n):
                        ln = struct.unpack("!i", p[off:off + 4])[0]
                        off += 4
                        if ln < 0:
                            r.append(None)
                        else:
                            r.append(p[off:off + ln].decode())
                            off += ln
                    rows.append(r)
                elif t == b"E":
                    got_err = errcode(p)
                elif t == b"Z":
                    break
            return got_err, rows

        nfail = 0
        def check(name, cond, detail=""):
            nonlocal nfail
            if not cond:
                nfail += 1
                print(f"FAIL {name} {detail}")

        # ---- setup ----
        e, _ = simple("create table mc89 (a int, b int)")
        check("create", e is None, e)
        e, _ = simple("insert into mc89 values (3, 1), (1, 2), (2, 3), (null, 4)")
        check("insert", e is None, e)

        # A. USING btree on every shape
        e, _ = simple("create index mc89_u1 on mc89 using btree (a)")
        check("using btree plain", e is None, e)
        e, _ = simple("create index mc89_u2 on mc89 using btree (a desc)")
        check("using btree desc", e is None, e)
        e, _ = simple("create index mc89_u3 on mc89 using btree (b desc nulls first)")
        check("using btree desc nulls first", e is None, e)
        e, _ = simple("create unique index mc89_u4 on mc89 using btree (a, b)")
        check("using btree unique multi", e is None, e)
        e, _ = simple("create index mc89_u5 on mc89 using btree ((a + b))")
        check("using btree expr", e is None, e)
        e, _ = simple("create index mc89_u6 on mc89 using btree (b) where b > 1")
        check("using btree partial", e is None, e)
        e, _ = simple("create index on mc89 using btree (a)")
        check("using btree auto-named", e is None, e)
        # non-btree methods are 0A000 and create nothing
        e, _ = simple("create index mc89_h on mc89 using hash (a)")
        check("using hash 0A000", e == "0A000", e)
        e, _ = simple("drop index mc89_h")
        check("hash created nothing", e == "42P01", e)
        e, _ = simple("create index mc89_g on mc89 using gist (a)")
        check("using gist 0A000", e == "0A000", e)

        # B. multi-name DROP INDEX atomicity
        e, _ = simple("drop index mc89_u1, mc89_nope")
        check("existing-first 42P01", e == "42P01", e)
        e, _ = simple("drop index mc89_u1")
        check("u1 survived abort", e is None, e)
        e, _ = simple("drop index mc89_nope, mc89_u2")
        check("missing-first 42P01", e == "42P01", e)
        e, _ = simple("drop index mc89_u2")
        check("u2 survived abort", e is None, e)
        e, _ = simple("drop index if exists mc89_u3, mc89_nope")
        check("if exists drops rest", e is None, e)
        e, _ = simple("drop index mc89_u3")
        check("u3 actually dropped", e == "42P01", e)

        # C. DML + ORDER BY correctness with USING btree indexes present
        e, _ = simple("insert into mc89 values (5, 5)")
        check("insert", e is None, e)
        e, r = simple("select a from mc89 order by a")
        check("order asc", e is None and r == [["1"], ["2"], ["3"], ["5"], [None]], (e, r))
        e, r = simple("select a from mc89 order by a desc")
        check("order desc", e is None and r == [[None], ["5"], ["3"], ["2"], ["1"]], (e, r))
        e, _ = simple("update mc89 set b = 9 where a = 5")
        check("update", e is None, e)
        e, _ = simple("delete from mc89 where a = 5")
        check("delete", e is None, e)

        # D. checkpoint with USING btree metadata present
        e, _ = simple("checkpoint")
        check("checkpoint", e is None, e)
        e, _ = simple("drop index mc89_u4, mc89_u5, mc89_u6")
        check("drop rest", e is None, e)

        s.sendall(b"X" + struct.pack("!i", 4))
        s.close()
        print(f"checks failed: {nfail}")
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=120)
        except Exception:
            proc.kill()
    with open(log) as f:
        txt = f.read()
    errors = 0
    for line in txt.splitlines():
        if "ERROR SUMMARY" in line:
            print(line.strip())
            try:
                errors = int(line.split()[3])
            except Exception:
                pass
    for line in txt.splitlines():
        if "definitely lost" in line or "indirectly lost" in line:
            print(line.strip())
    print(f"VALGRIND_ERRORS={errors} SQL_UNEXPECTED={nfail}")
    sys.exit(1 if (errors or nfail) else 0)


if __name__ == "__main__":
    main()
