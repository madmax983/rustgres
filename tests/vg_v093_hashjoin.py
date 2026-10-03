#!/usr/bin/env python3
"""callgrind/dhat driver for the v0.93 hash join.

Starts the release server under valgrind --tool=callgrind (or dhat),
loads N x N rows, runs the equi-join, shuts down, and prints the
instruction count (callgrind) or heap summary (dhat).

Usage: python3 tests/vg_v093_hashjoin.py [--tool callgrind|dhat] [--n 10000]
       [--no-hash]  (sets RUSTGRES_NO_HASH_JOIN=1 for the nested baseline)
"""
import os, socket, struct, subprocess, sys, tempfile, time, re

PORT = 5594
VG = os.path.expanduser("~/workspace/valgrind-local/valgrind")
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                   "..", "target", "release", "rustgres")


def main():
    tool = "callgrind"
    n = 10000
    no_hash = False
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--tool" and i + 1 < len(args):
            tool = args[i + 1]; i += 2
        elif a == "--n" and i + 1 < len(args):
            n = int(args[i + 1]); i += 2
        elif a == "--no-hash":
            no_hash = True; i += 1
        else:
            i += 1
    data_dir = tempfile.mkdtemp(prefix="rgvg93_")
    out = os.path.join(data_dir, "vg.out")
    cmd = [VG, f"--tool={tool}", f"--log-file={out}.%p"]
    if tool == "callgrind":
        cmd += ["--callgrind-out-file=" + out + ".cg"]
    env = dict(os.environ)
    if no_hash:
        env["RUSTGRES_NO_HASH_JOIN"] = "1"
    proc = subprocess.Popen(
        cmd + [BIN, "--data-dir", data_dir, "--port", str(PORT)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
                s.close(); break
            except OSError:
                time.sleep(0.5)
        else:
            print("no start"); proc.kill(); sys.exit(2)
        s = socket.create_connection(("127.0.0.1", PORT), timeout=600)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        s.sendall(struct.pack("!i", len(body) + 4) + body)

        def rd(k):
            d = b""
            while len(d) < k:
                c = s.recv(k - len(d))
                if not c: raise RuntimeError("closed")
                d += c
            return d
        def msg():
            t = rd(1); ln = struct.unpack("!I", rd(4))[0]; return t, rd(ln - 4)
        while True:
            t, _ = msg()
            if t == b"Z": break
        def q(sql):
            s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1)
                      + sql.encode() + b"\x00")
            while True:
                t, _ = msg()
                if t == b"Z": return
        q("create table va(id int);")
        q("create table vb(id int);")
        batch = 1000
        for lo in range(1, n + 1, batch):
            hi = min(lo + batch - 1, n)
            vals = ",".join(f"({k})" for k in range(lo, hi + 1))
            q(f"insert into va values {vals};")
            q(f"insert into vb values {vals};")
        t0 = time.perf_counter()
        q("select count(*), sum(va.id) from va join vb on va.id = vb.id;")
        dt = time.perf_counter() - t0
        print(f"join wall: {dt:.2f}s (n={n}, tool={tool}, "
              f"hash={'off' if no_hash else 'on'})", flush=True)
        s.close()
    finally:
        proc.terminate()
        try: proc.wait(timeout=60)
        except subprocess.TimeoutExpired: proc.kill()
    # summarize
    if tool == "callgrind":
        import glob
        cgs = glob.glob(out + ".cg*")
        if cgs:
            txt = open(cgs[0], errors="replace").read(200000)
            m = re.search(r"summary: (\d+)", txt)
            print("callgrind summary instructions:",
                  m.group(1) if m else "?")
    else:
        logs = [f for f in os.listdir(data_dir) if f.startswith("vg.out")]
        if logs:
            txt = open(os.path.join(data_dir, logs[0]),
                       errors="replace").read()
            for pat in [r"Total:\s+([\d,]+ bytes.*)",
                        r"At 0x.*: .*",
                        r"Maximum.*live.*",
                        r"At t-gmax.*",
                        r"At t-end.*"]:
                pass
            # print the dhat heap summary block
            idx = txt.find("dhat: Total:")
            if idx >= 0:
                print(txt[idx:idx + 1200])


if __name__ == "__main__":
    main()
