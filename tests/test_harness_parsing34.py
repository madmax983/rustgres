#!/usr/bin/env python3
"""v0.34 focused tests for conformance harness expected-output parsing.

Covers the three v0.34 regress_runner.py repairs:
1. canon() strips text/date/timestamp actuals (symmetric with the
   always-stripped expected side).
2. resolve_physical_rows(): blank physical lines are empty-string data rows
   when the "(N rows)" count says so; separators otherwise.
3. resolve_physical_rows(): a single-column single-row value spanning
   several physical lines (embedded newlines, e.g. wrapped base64) is
   re-joined with "\\n" instead of becoming phantom rows.
"""
import sys
sys.path.insert(0, "tests/conformance")
from regress_runner import canon, parse_expected_block, resolve_physical_rows

passed = failed = 0
def check(name, cond):
    global passed, failed
    if cond:
        passed += 1
    else:
        failed += 1
        print(f"FAIL {name}")

# 1. canon() stripping symmetry -------------------------------------------
check("canon strips text trailing spaces", canon(25, "hi   ") == "hi")
check("canon strips text leading spaces", canon(25, "   hi") == "hi")
check("canon leaves empty string empty", canon(25, "") == "")
check("canon strips whitespace-only to empty", canon(25, "   ") == "")
check("canon None stays None", canon(25, None) is None)
check("canon unknown oid strips too", canon(1043, " a ") == "a")

# 2. blank lines as data rows vs separators --------------------------------
def cells1(ln):
    # single column: whole line stripped (test-local widths)
    return [ln.strip()]

# one-row empty string: [""] + "(1 row)" -> [[""]]
check("blank line is empty-string row when count=1",
      resolve_physical_rows([""], 1, cells1, 1) == [[""]])
# two blanks + "(2 rows)" -> two empty rows
check("two blanks are two empty rows when count=2",
      resolve_physical_rows(["", ""], 2, cells1, 1) == [[""], [""]])
# data + blank separator + "(1 row)" -> blank skipped
check("blank is separator when nonblank matches count",
      resolve_physical_rows(["ab", ""], 1, cells1, 1) == [["ab"]])
# no count marker -> old separator-skipping behavior
check("no count marker skips blanks",
      resolve_physical_rows(["ab", ""], None, cells1, 1) == [["ab"]])
# mixed: ["a", "", "b"] + "(3 rows)" -> all three are rows
check("interior blank is a row when count=3",
      resolve_physical_rows(["a", "", "b"], 3, cells1, 1) == [["a"], [""], ["b"]])

# 3. multi-line single-column values ----------------------------------------
multi = ["EjRWeJCrze8AARI0VniQq83vAAESNFZ4kKvN", "7wABEjRWeJCrze8AAQ=="]
got = resolve_physical_rows(multi, 1, cells1, 1)
check("multiline joins with newline",
      got == [["EjRWeJCrze8AARI0VniQq83vAAESNFZ4kKvN\n7wABEjRWeJCrze8AAQ=="]])
# multi-column with extra lines: not disambiguable -> fallback (skip blanks)
def cells2(ln):
    return [ln[0:4].strip(), ln[5:9].strip()]
check("multicol extra lines falls back",
      resolve_physical_rows(["a    x", "b    y"], 1, cells2, 2) == [["a", "x"], ["b", "y"]])

# 4. parse_expected_block integration ---------------------------------------
def parse(lines):
    exp, _ = parse_expected_block(lines, 0, "")
    return exp

# empty-string one-row result
exp = parse([" foo ", "-----", "", "(1 row)"])
check("block: empty-string row parsed", exp.kind == "table" and exp.rows == [[""]])
# trailing-space value keeps working via canon on the actual side only;
# here we just check the expected side strips (unchanged behavior)
exp = parse([" foo ", "-----", " hi  ", "(1 row)"])
check("block: value cell stripped", exp.rows == [["hi"]])
# wrapped base64: two physical lines, one logical row
exp = parse([" encode ", "--------------------------------------",
             " EjRWeJCrze8AARI0VniQq83vAAESNFZ4kKvN",
             " 7wABEjRWeJCrze8AAQ==",
             "(1 row)"])
check("block: wrapped value rejoined",
      exp.rows == [["EjRWeJCrze8AARI0VniQq83vAAESNFZ4kKvN\n7wABEjRWeJCrze8AAQ=="]])

print(f"{passed}/{passed+failed} passed")
sys.exit(1 if failed else 0)
