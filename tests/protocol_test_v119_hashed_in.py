#!/usr/bin/env python3
r"""v1.19 protocol tests: hashed IN-subquery with JOIN in the subquery.

Covers (simple protocol):
- `x IN (SELECT ... FROM a JOIN b ...)` uses the hashed path (v1.19:
  `match_hashable_in` now accepts JOINs, not just single tables; the
  pre-hashed-path TOO_SLOW mask for IN-subqueries is removed)
- Correct results for IN with JOIN, including three-valued NULL logic
- `NOT IN` with JOIN
- Performance: 10k x 10k IN completes quickly (hashed, not O(n^2))

Self-starting: launches rustgres on 5599 with a fresh datadir.
Uses a uniquely-named binary copy to avoid pkill collisions.
"""
import socket, struct, subprocess, sys, time, os, shutil

PORT = 5599
HERE = os.path.dirname(os.path.abspath(__file__))
SRC_BIN = os.path.join(HERE, "..", "target", "debug", "rustgres")
SCRATCH = os.path.expanduser(
    "~/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v119"
)
BIN = os.path.join(SCRATCH, "rg-v119-proto-bin")
DATA = os.path.join(SCRATCH, "rg-v119-proto-data")


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
    s.sendall(b"Q" + struct.pack("!I", len(sql) + 5) + sql.encode() + b"\x00")
    rows = []
    err = None
    while True:
        t, p = read_msg(s)
        if t == b"T":
            pass
        elif t == b"D":
            n = struct.unpack("!h", p[:2])[0]
            pos = 2
            vals = []
            for _ in range(n):
                flen = struct.unpack("!i", p[pos : pos + 4])[0]
                pos += 4
                if flen < 0:
                    vals.append(None)
                else:
                    vals.append(p[pos : pos + flen].decode())
                    pos += flen
            rows.append(tuple(vals))
        elif t == b"E":
            try:
                err = p.split(b"M")[1].split(b"\x00")[0].decode()
            except Exception:
                err = "<unparseable>"
        elif t == b"Z":
            break
    return rows, err


def main():
    os.makedirs(SCRATCH, exist_ok=True)
    shutil.copy2(SRC_BIN, BIN)
    os.chmod(BIN, 0o755)
    shutil.rmtree(DATA, ignore_errors=True)
    proc = subprocess.Popen(
        [BIN, "--port", str(PORT), "--data-dir", DATA],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(2)
    fails = []
    try:
        s = connect()
        # setup
        q(s, "CREATE TABLE ha(a int)")
        q(s, "INSERT INTO ha SELECT generate_series(1,1000)")
        q(s, "CREATE TABLE hb(b int, c int)")
        q(s, "INSERT INTO hb SELECT g, g % 10 FROM generate_series(1,1000) g")

        # 1. IN with JOIN in subquery
        rows, err = q(
            s,
            "SELECT count(*) FROM ha WHERE a IN "
            "(SELECT b FROM hb JOIN ha h2 ON hb.b = h2.a WHERE hb.c = 5)",
        )
        # hb.c=5 matches b=5,15,...,995 (100 rows); all are in ha (1..1000)
        if err or rows != [("100",)]:
            fails.append(f"in-join: {rows} {err}")

        # 2. NOT IN with JOIN
        rows, err = q(
            s,
            "SELECT count(*) FROM ha WHERE a NOT IN "
            "(SELECT b FROM hb JOIN ha h2 ON hb.b = h2.a WHERE hb.c = 5)",
        )
        if err or rows != [("900",)]:
            fails.append(f"not-in-join: {rows} {err}")

        # 3. NULL semantics: subquery with NULL
        q(s, "INSERT INTO hb VALUES (NULL, 5)")
        rows, err = q(s, "SELECT 5 IN (SELECT b FROM hb WHERE c = 5)")
        # 5 is in the set -> true (even though NULL is also in the set)
        if err or rows != [("t",)]:
            fails.append(f"in-null-true: {rows} {err}")
        rows, err = q(s, "SELECT 2000 IN (SELECT b FROM hb WHERE c = 5)")
        # 2000 not in set, NULL in set -> NULL (unknown)
        if err or rows != [(None,)]:
            fails.append(f"in-null-unknown: {rows} {err}")

        # 4. Performance: 10k x 10k should be fast (hashed)
        q(s, "CREATE TABLE big1(x int)")
        q(s, "INSERT INTO big1 SELECT generate_series(1,10000)")
        q(s, "CREATE TABLE big2(y int)")
        q(s, "INSERT INTO big2 SELECT generate_series(1,10000)")
        t0 = time.time()
        rows, err = q(
            s,
            "SELECT count(*) FROM big1 WHERE x IN "
            "(SELECT y FROM big2 JOIN big1 b2 ON big2.y = b2.x)",
        )
        dt = time.time() - t0
        if err or rows != [("10000",)]:
            fails.append(f"perf-result: {rows} {err}")
        if dt > 10:
            fails.append(f"perf-slow: {dt:.1f}s > 10s (not hashed?)")
        print(f"  10k x 10k JOIN IN: {dt:.2f}s", flush=True)
        s.close()
    finally:
        proc.terminate()
        proc.wait()
    if fails:
        print("FAILURES:", fails)
        return 1
    print("v1.19 protocol: 0 failures")
    return 0


if __name__ == "__main__":
    sys.exit(main())
