#!/usr/bin/env python3
r"""Serial two-phase protocol runner (v0.94).

The 2026-09-25 incident report: a wildcard concurrent run mixed
self-starting suites (which bind their own ports) with prestarted
suites (which expect an already-running server on 5433), and ran them
against an occupied port — producing order-dependent failures that
looked like real regressions.

This runner enforces the correct sequencing:

  Phase 1: SELF suites, one at a time, serially. Each suite starts and
           stops its own server. Nothing else may hold the suite ports
           (5432/5433/5442/5443 are checked free up front).
  Phase 2: ONE fresh server on 5433 with a fresh datadir, then all PRE
           (prestarted-expecting) suites run serially against it, then
           the server is shut down.

Usage: python3 tests/protocol_run_all.py [--only SUBSTR] [--phase 1|2]

Exit 0 only if every suite exits 0. Per-suite timeout: 600s.
"""
import glob, os, re, socket, subprocess, sys, time

TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(os.path.dirname(TESTS_DIR), "target", "debug", "rustgres")
PORT = 5433
SUITE_TIMEOUT = 600


def suite_key(path):
    name = os.path.basename(path)
    m = re.match(r"protocol_test(?:_v(\d+))?(?:_(\w+))?\.py$", name)
    if m and m.group(1):
        return (1_000_000 + int(m.group(1)), m.group(2) or "")
    if m and m.group(2):
        return (int(m.group(2)) if m.group(2).isdigit() else 999999, "")
    return (999999, name)


def classify(path):
    with open(path, "rb") as f:
        return "SELF" if b"subprocess.Popen" in f.read() else "PRE"


def port_free(port):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def wait_for_server(port, tries=100):
    for _ in range(tries):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.2)
    return False


def run_suite(path, only):
    name = os.path.basename(path)
    if only and only not in name:
        return None
    try:
        proc = subprocess.run(
            [sys.executable, path],
            cwd=os.path.dirname(TESTS_DIR),
            capture_output=True, text=True, timeout=SUITE_TIMEOUT)
        last = (proc.stdout or "").strip().splitlines()
        summary = last[-1] if last else "(no output)"
        if proc.returncode == 0:
            return ("PASS", summary)
        err_tail = (proc.stderr or "").strip().splitlines()
        detail = err_tail[-1] if err_tail else summary
        return ("FAIL", f"{summary} :: {detail}")
    except subprocess.TimeoutExpired:
        return ("TIMEOUT", f"exceeded {SUITE_TIMEOUT}s")
    except Exception as e:  # noqa: BLE001
        return ("ERROR", str(e))


def main():
    only = None
    phases = (1, 2)
    args = sys.argv[1:]
    while args:
        a = args.pop(0)
        if a == "--only":
            only = args.pop(0)
        elif a == "--phase":
            phases = (int(args.pop(0)),)

    if not os.path.exists(BIN):
        print(f"FATAL: debug binary missing: {BIN} (run cargo build first)")
        sys.exit(2)

    paths = sorted(glob.glob(os.path.join(TESTS_DIR, "protocol_test*.py")),
                   key=suite_key)
    paths = [p for p in paths if os.path.basename(p) != "protocol_run_all.py"]
    by_phase = {1: [], 2: []}
    for p in paths:
        by_phase[1 if classify(p) == "SELF" else 2].append(p)
    print(f"suites: phase1(SELF)={len(by_phase[1])} phase2(PRE)={len(by_phase[2])}"
          + (f" only={only!r}" if only else ""))

    results = {}

    if 1 in phases:
        for port in (5432, 5433, 5442, 5443):
            if not port_free(port):
                print(f"FATAL: port {port} is occupied; refusing to run SELF "
                      f"suites against an occupied port")
                sys.exit(2)
        for p in by_phase[1]:
            r = run_suite(p, only)
            if r is None:
                continue
            results[p] = r
            print(f"[phase1] {os.path.basename(p):42s} {r[0]:7s} {r[1][:110]}")
            # v0.98: SELF suites each bind their own port; give the just-
            # stopped server a moment to release it before the next suite
            # binds (avoids transient "port already in use" flakes when
            # two consecutive suites share a port).
            time.sleep(2)

    if 2 in phases:
        datadir = "/tmp/rg_proto_phase2"
        os.system(f"rm -rf {datadir}")
        if not port_free(PORT):
            print(f"FATAL: port {PORT} is occupied; cannot start phase-2 server")
            sys.exit(2)
        proc = subprocess.Popen(
            [BIN, "--port", str(PORT), "--data-dir", datadir],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            if not wait_for_server(PORT):
                print("FATAL: phase-2 server never came up")
                sys.exit(2)
            for p in by_phase[2]:
                r = run_suite(p, only)
                if r is None:
                    continue
                results[p] = r
                print(f"[phase2] {os.path.basename(p):42s} {r[0]:7s} {r[1][:110]}")
        finally:
            proc.terminate()
            proc.wait()

    fails = [(p, r) for p, r in results.items() if r[0] != "PASS"]
    n_pass = len(results) - len(fails)
    print(f"\nprotocol_run_all: {n_pass}/{len(results)} suites PASS; "
          f"{len(fails)} non-pass")
    for p, (st, detail) in fails:
        print(f"  {st:7s} {os.path.basename(p)}: {detail[:160]}")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
