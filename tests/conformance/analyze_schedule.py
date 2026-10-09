#!/usr/bin/env python3
"""Categorize schedule_runner.py failures into a feature-gap ranking.

Reads the --json report and buckets every non-cascade REAL-FAIL by what
it says is missing (a type, a function, a syntax form, a catalog, a GUC)
or by how it went wrong (wrong rows, error expected but succeeded, ...).
EXPLAIN statements get their own bucket: they measure plan-text fidelity,
not functionality.

Usage: python3 tests/conformance/analyze_schedule.py pg19.json [--md OUT.md]
"""

import json
import re
import sys
from collections import Counter, defaultdict


def first_words(stmt, n=2):
    s = re.sub(r"(?s)^\s*(--[^\n]*\n\s*)*", "", stmt)
    return " ".join(s.split()[:n]).upper()


def syntax_key(stmt, err):
    """Statement kind plus the token the parser choked on, e.g.
    `CREATE … unlogged` or `SELECT … jsonb`."""
    m = (re.search(r'at or near "([^"]+)"', err)
         or re.search(r'found (?:Ident|Keyword)\("?([^")]+)"?\)', err)
         or re.search(r"found (\w+)", err)
         or re.search(r"unexpected (?:Ident\(\")?([\w]+)", err))
    if m:
        return "%s … %s" % (first_words(stmt, 1), m.group(1).lower())
    # No offending token in the message ("expected TABLE, ... after
    # ALTER"): key on the object type, plus the action for ALTER TABLE.
    words = re.sub(r"(?s)^\s*(--[^\n]*\n\s*)*", "", stmt).upper().split()
    words = [w for w in words if w not in ("ONLY", "IF", "EXISTS", "OR", "REPLACE")]
    if len(words) >= 4 and words[:2] == ["ALTER", "TABLE"]:
        return "ALTER TABLE … %s" % " ".join(words[3:5])
    return " ".join(words[:2])


def categorize(f):
    """Return (bucket, key) for one failure record."""
    stmt, detail, err = f["stmt"], f["detail"], f.get("error") or ""
    if detail.startswith("cascade:"):
        return ("cascade", detail.split(";")[0][len("cascade: "):].split(" ")[0])
    if detail.startswith("server:"):
        return ("server crash/timeout", first_words(stmt))
    if re.match(r"(?is)^\s*(--[^\n]*\n\s*)*explain\b", stmt):
        return ("EXPLAIN plan text", "EXPLAIN")
    m = re.search(r'type "([^"]+)" does not exist', err)
    if m:
        return ("missing type", m.group(1).lower())
    m = re.search(r"function ([\w.]+)\(.*\) does not exist", err)
    if m:
        return ("missing function", m.group(1).lower())
    m = re.search(r"operator does not exist: (.+)", err)
    if m:
        return ("missing operator", re.sub(r"\s+", " ", m.group(1))[:60])
    m = re.search(r'relation "(pg_\w+|information_schema\.\w+)" does not exist', err)
    if m:
        return ("missing catalog", m.group(1))
    m = re.search(r'column "(\w+)" does not exist', err)
    if m and re.search(r"\bpg_\w+", stmt):
        return ("catalog column", m.group(1))
    m = re.search(r'unrecognized configuration parameter "([^"]+)"', err)
    if m:
        return ("missing GUC", m.group(1))
    if "SQLSTATE 25P02" in detail:
        return ("knock-on: statement in aborted transaction", first_words(stmt, 1))
    if "SQLSTATE 42P01" in detail:
        return ("knock-on: relation missing (probable cascade)",
                (re.search(r'"([^"]+)"', err) or re.match("(.*)", "?")).group(1))
    if ("syntax error" in err or "42601" in detail) and re.search(
            r"(found|unexpected) Colon|at or near \":\"", err) and re.search(
            r"(?<!:):['\"]?[A-Za-z_]", stmt):
        # A psql :variable left verbatim: the \gset that should have set
        # it failed earlier.
        return ("knock-on: psql variable never set (earlier \\gset failed)",
                first_words(stmt, 1))
    if "syntax error" in err or "42601" in detail:
        return ("unsupported syntax", syntax_key(stmt, err))
    if "0A000" in detail:
        return ("feature not supported (0A000)", first_words(stmt))
    if detail.startswith("expected ERROR") or detail.startswith("fewer result sets"):
        return ("accepted invalid input (no error)", first_words(stmt))
    if detail.startswith("row mismatch") or detail.startswith("column"):
        return ("wrong result", first_words(stmt))
    m = re.search(r"SQLSTATE (\w+)", detail)
    if m:
        return ("unexpected error " + m.group(1), (err or first_words(stmt))[:60])
    return ("other", detail[:60])


def main():
    path = sys.argv[1]
    md = sys.argv[sys.argv.index("--md") + 1] if "--md" in sys.argv else None
    data = json.load(open(path))
    buckets = Counter()
    keys = defaultdict(Counter)
    suites = defaultdict(set)
    for suite, s in data["suites"].items():
        for f in s["failures"]:
            if f["status"] != "REAL-FAIL":
                continue
            b, k = categorize(f)
            buckets[b] += 1
            keys[b][k] += 1
            suites[(b, k)].add(suite)
    lines = []
    t = data["totals"]
    scored = t.get("PASS", 0) + t.get("EXPECTED-FAIL", 0) + t.get("REAL-FAIL", 0)
    lines.append("Scored %d: PASS %d (%.1f%%), EXPECTED-FAIL %d, REAL-FAIL %d "
                 "(cascade %d)\n" % (scored, t.get("PASS", 0),
                                     100.0 * t.get("PASS", 0) / max(scored, 1),
                                     t.get("EXPECTED-FAIL", 0), t.get("REAL-FAIL", 0),
                                     t.get("CASCADE", 0)))
    lines.append("## REAL-FAIL by category\n")
    lines.append("| category | count |")
    lines.append("|---|---:|")
    for b, c in buckets.most_common():
        lines.append("| %s | %d |" % (b, c))
    for b, _ in buckets.most_common():
        if b == "cascade" or b.startswith("knock-on"):
            continue
        lines.append("\n### %s (top 25)\n" % b)
        lines.append("| key | count | suites |")
        lines.append("|---|---:|---|")
        for k, c in keys[b].most_common(25):
            ss = sorted(suites[(b, k)])
            lines.append("| `%s` | %d | %s |" % (
                k.replace("|", "\\|"), c,
                ", ".join(ss[:6]) + (" +%d" % (len(ss) - 6) if len(ss) > 6 else "")))
    out = "\n".join(lines) + "\n"
    if md:
        open(md, "w").write(out)
    print(out)


if __name__ == "__main__":
    main()
