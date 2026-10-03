#!/usr/bin/env python3
"""tally_expected_fails.py — v1.17 scoping: run the full conformance
machinery per suite and tally EXPECTED-FAIL verdicts by documented
reason (the `detail` carried on each Verdict). Writes a JSON + text
summary under hidden_files/v117/.

Reuses regress_runner.run_test / setup_statements verbatim so the
classification is identical to the real run.
"""
import importlib.util
import json
import os
import sys
import time
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(HERE, "..", "..")
OUT = "/home/hatch/workspace/goals/rustgres-rust-postgres-compatible-server/hidden_files/v117"
spec = importlib.util.spec_from_file_location(
    "rr", os.path.join(ROOT, "tests", "conformance", "regress_runner.py"))
rr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rr)


def main():
    if not os.path.exists(rr.BIN):
        print("missing binary; run cargo build first")
        return 2
    tests = [(n, t, o, r) for n, t, o, r in rr.TESTS]
    need_tenk_any = any(t for _, t, _, _ in tests)
    need_onek_any = any(o for _, _, o, _ in tests)
    need_road_any = any(r for _, _, _, r in tests)

    reason_counts = Counter()
    reason_suites = defaultdict(set)
    reason_examples = {}
    totals = Counter()

    server = rr.Server()
    server.start()
    wedged = {}
    try:
        for name, need_tenk, need_onek, need_road in tests:
            server.stop()
            server = rr.Server()
            server.start()
            conn = rr.Conn()
            setup = rr.setup_statements(need_tenk_any, need_onek_any, need_road_any)
            bad = None
            for s in setup:
                r = conn.q(s)
                if r["err_codes"]:
                    bad = s
                    break
            if bad:
                print("SETUP FAILED on %s: %s" % (name, bad[:60]), flush=True)
                return 2
            restarts = 0
            while True:
                try:
                    results, err = rr.run_test(
                        conn, name, need_tenk, verbose=False, skip_stmts=wedged)
                    break
                except rr.ServerWedged as w:
                    restarts += 1
                    if restarts > 10:
                        print("FATAL wedged %s" % name)
                        return 2
                    wedged[w.stmt] = w.reason
                    server.stop()
                    server = rr.Server()
                    server.start()
                    conn = rr.Conn()
                    for s in setup:
                        conn.q(s)
            if err:
                print("FATAL %s: %s" % (name, err))
                return 2
            for stmt, v in results:
                if v.status == "SKIP":
                    continue
                totals[v.status] += 1
                if v.status == "EXPECTED-FAIL":
                    reason = v.detail.split(" [")[0]
                    reason_counts[reason] += 1
                    reason_suites[reason].add(name)
                    if reason not in reason_examples:
                        reason_examples[reason] = stmt[:120].replace("\n", " ")
            print("done %s" % name, flush=True)
    finally:
        server.stop()

    rows = []
    for reason, c in reason_counts.most_common():
        rows.append({
            "count": c,
            "reason": reason,
            "suites": sorted(reason_suites[reason]),
            "example": reason_examples[reason],
        })
    summary = {
        "totals": dict(totals),
        "expected_fail_total": sum(reason_counts.values()),
        "by_reason": rows,
    }
    with open(os.path.join(OUT, "expected_fail_tally.json"), "w") as f:
        json.dump(summary, f, indent=2)
    with open(os.path.join(OUT, "expected_fail_tally.txt"), "w") as f:
        f.write("totals: %s\n\n" % dict(totals))
        for r in rows:
            f.write("%4d | %s\n       suites=%s\n       e.g. %s\n" % (
                r["count"], r["reason"], ",".join(r["suites"]), r["example"]))
    print("totals: %s" % dict(totals))
    for r in rows[:30]:
        print("%4d | %s | %s" % (r["count"], r["reason"], ",".join(r["suites"])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
