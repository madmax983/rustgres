#!/usr/bin/env python3
"""Self-contained callgrind/dhat driver for v2.38's jsonb subscript
assignment (`UPDATE t SET data['path'] = v`, src/exec/json.rs
jsonb_subscript_assign / parse_jsonb / render) -- never profiled since
v2.38 added it.

Realistic shape: a "patch one field of a JSON document" bulk UPDATE --
the textbook use case subscript assignment exists for (PG19's own docs
example is exactly `UPDATE t SET data['tags'] = ...`), run over many
rows each holding a DIFFERENT, moderately nested jsonb document (an
order record: top-level scalars, a nested object, an array). Each row
is updated through one subscript path (`data['meta']['score'] = id`),
mirroring a "bump one derived field per row" batch job.

Every assignment calls __jsonb_subscript_assign ->
Database-column-value Value::Jsonb(String) -> jsonb_val_arg ->
parse_jsonb(text) [text->tree, memoized by a thread-local
JSONB_PARSE_CACHE keyed on the FULL document text] -> assign_rec (tree
edit) -> JsonbVal::render() [tree->text]. Because every row's document
text is distinct (real per-row data, not a repeated constant), the
parse cache can never hit for this workload: every lookup is a
guaranteed miss that still pays a full-string hash, and every miss
inserts a full clone of the text (as the key) and a full clone of the
parsed tree (as the value, cap 64) that will never be read back.

This harness isolates the subscript-assign cost two ways:
  * `--op subscript` vs `--op plain`: `SET data['meta']['score'] = id`
    vs `SET id = id` (same row count, same table, no jsonb touched) --
    diffing the two isolates the jsonb-assign-path cost from generic
    per-row UPDATE/WAL/pgstat overhead.
  * `--docsize N` pads each row's jsonb document with N extra
    object keys (each holding a short string), so the parse/render
    cost's scaling with document size is visible.

Usage:
    cargo build
    python3 benches/profile_jsonb_subscript.py --tool callgrind --rows 2000 --docsize 10
    python3 benches/profile_jsonb_subscript.py --tool callgrind --rows 2000 --docsize 10 --op plain
    python3 benches/profile_jsonb_subscript.py --tool dhat --rows 2000 --docsize 10

    # scan the row-count and doc-size axes in one go, printing Ir:
    python3 benches/profile_jsonb_subscript.py --tool callgrind --row-sizes
    python3 benches/profile_jsonb_subscript.py --tool callgrind --doc-sizes
"""
import argparse
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
DEFAULT_ROW_SIZES = [500, 1000, 2000]
DEFAULT_DOC_SIZES = [0, 10, 40]


def find_vg():
    vg = shutil.which("valgrind")
    if not vg:
        print("valgrind not found on PATH", file=sys.stderr)
        sys.exit(1)
    return vg


def free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class Wire:
    """Minimal simple-query client (startup + Query + sync wait)."""

    def __init__(self, port, timeout=600):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        while True:
            t, _ = self._msg()
            if t == b"Z":
                break

    def _rd(self, n):
        d = b""
        while len(d) < n:
            c = self.s.recv(n - len(d))
            if not c:
                raise RuntimeError("connection closed")
            d += c
        return d

    def _msg(self):
        t = self._rd(1)
        ln = struct.unpack("!i", self._rd(4))[0]
        return t, self._rd(ln - 4)

    def simple(self, q):
        qb = q.encode()
        self.s.sendall(struct.pack("!c", b"Q") + struct.pack("!i", len(qb) + 5) + qb + b"\x00")
        msgs = []
        while True:
            t, p = self._msg()
            msgs.append((t, p))
            if t == b"Z":
                return msgs
            if t == b"E":
                raise RuntimeError(p)

    def close(self):
        self.s.close()


def doc_for(i, docsize):
    """A distinct, moderately nested jsonb document for row i: a
    top-level scalar, a nested object with a nested array, and
    `docsize` extra padding keys (so no two rows ever share text)."""
    extra = ",".join(f'"k{j}":"v{i}_{j}"' for j in range(docsize))
    extra = ("," + extra) if extra else ""
    return (
        f'{{"name":"item-{i}",'
        f'"meta":{{"score":{i},"tags":[{i},{i+1},{i+2}]}}{extra}}}'
    )


def run_one(tool, rows, docsize, op, chunk=200, keep_log=None):
    vg = find_vg()
    port = free_port()
    data_dir = tempfile.mkdtemp(prefix="rgjs_")
    log_dir = tempfile.mkdtemp(prefix="rgjs_log_")
    log = os.path.join(log_dir, f"{tool}.log")
    args = [vg, f"--tool={tool}", f"--log-file={log}"]
    if tool == "callgrind":
        args += [
            "--callgrind-out-file=" + os.path.join(log_dir, "callgrind.out"),
            "--collect-jumps=yes",
            "--cache-sim=yes",
            "--branch-sim=yes",
        ]
    elif tool == "dhat":
        args += ["--dhat-out-file=" + os.path.join(log_dir, "dhat.out")]
    proc = subprocess.Popen(
        args + [BIN, "--data-dir", data_dir, "--port", str(port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(900):
            try:
                s = socket.create_connection(("127.0.0.1", port), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            raise RuntimeError("server did not start")

        w = Wire(port, timeout=900)
        w.simple("CREATE TABLE t (id int, data jsonb)")
        for start in range(0, rows, chunk):
            end = min(start + chunk, rows)
            vals = ",".join(f"({i},'{doc_for(i, docsize)}')" for i in range(start, end))
            w.simple(f"INSERT INTO t VALUES {vals}")

        # Measured statement: one bulk UPDATE touching every row.
        if op == "subscript":
            w.simple("UPDATE t SET data['meta']['score'] = id")
        else:
            w.simple("UPDATE t SET id = id")
        w.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except Exception:
            proc.kill()
        shutil.rmtree(data_dir, ignore_errors=True)

    result = {}
    if tool == "callgrind":
        cg_out = os.path.join(log_dir, "callgrind.out")
        with open(cg_out) as f:
            for line in f:
                if line.startswith("totals:"):
                    result["Ir"] = int(line.split(":")[1].strip().split()[0])
        result["path"] = cg_out
    elif tool == "dhat":
        result["path"] = os.path.join(log_dir, "dhat.out")
        with open(result["path"]) as f:
            txt = f.read()
        result["raw"] = txt
    if keep_log:
        shutil.copy(result["path"], keep_log)
        result["kept"] = keep_log
    else:
        shutil.rmtree(log_dir, ignore_errors=True)
    return result


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                  formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tool", choices=["callgrind", "dhat"], default="callgrind")
    ap.add_argument("--rows", type=int, default=2000,
                     help="rows in t, each updated via one subscript assign (default 2000)")
    ap.add_argument("--docsize", type=int, default=10,
                     help="extra padding keys per jsonb document (default 10)")
    ap.add_argument("--op", choices=["subscript", "plain"], default="subscript",
                     help="subscript = SET data['meta']['score']=id; "
                          "plain = SET id=id (baseline, same row count, no jsonb touch)")
    ap.add_argument("--row-sizes", action="store_true",
                     help=f"run --rows at {DEFAULT_ROW_SIZES} in one go (fixed --docsize)")
    ap.add_argument("--doc-sizes", action="store_true",
                     help=f"run --docsize at {DEFAULT_DOC_SIZES} in one go (fixed --rows)")
    ap.add_argument("--keep-log", default=None,
                     help="copy the raw profile to this path (single-run mode only)")
    args = ap.parse_args()

    if args.row_sizes:
        for n in DEFAULT_ROW_SIZES:
            r = run_one(args.tool, n, args.docsize, args.op)
            if args.tool == "callgrind":
                print(f"rows={n} docsize={args.docsize} op={args.op} Ir={r.get('Ir')}")
        return
    if args.doc_sizes:
        for n in DEFAULT_DOC_SIZES:
            r = run_one(args.tool, args.rows, n, args.op)
            if args.tool == "callgrind":
                print(f"rows={args.rows} docsize={n} op={args.op} Ir={r.get('Ir')}")
        return

    r = run_one(args.tool, args.rows, args.docsize, args.op, keep_log=args.keep_log)
    if args.tool == "callgrind":
        print(f"rows={args.rows} docsize={args.docsize} op={args.op} Ir={r.get('Ir')}")
    else:
        lines = [ln for ln in r["raw"].splitlines()
                 if "total:" in ln or "At t-end" in ln or "guest_insns" in ln]
        print(f"rows={args.rows} docsize={args.docsize} op={args.op}")
        for ln in lines[:6]:
            print(f"  {ln.strip()}")


if __name__ == "__main__":
    main()
