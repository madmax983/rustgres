#!/usr/bin/env python3
"""v1.04 unit tests: psql escaped-semicolon handling in the conformance
runner's statement splitter and query unescaper.

psql does not treat `\\;` as a query-buffer terminator: it sends the
whole thing as ONE simple-protocol Query with the backslashes removed.
The splitter must therefore not break an item at `\\;` (outside
strings/comments/dollar quotes), and `psql_unescape` must reproduce
exactly the bytes psql would send.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "conformance"))
from regress_runner import split_statements, psql_unescape, parse_expected_blocks

PASS = 0
FAIL = 0


def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"PASS {name}")
    else:
        FAIL += 1
        print(f"FAIL {name} {detail}")


def sql_items(text):
    return [t for kind, t in split_statements(text) if kind == "sql"]


def main():
    # The transactions.sql REAL-FAIL shape: one item, raw `\;` kept.
    items = sql_items("SELECT 1\\; SELECT 2\\; SELECT 3;\n")
    check("escaped semis: single item", len(items) == 1, f"{items}")
    check("escaped semis: raw backslashes kept",
          items[0] == "SELECT 1\\; SELECT 2\\; SELECT 3;", f"{items[0]!r}")
    check("unescape: psql wire bytes",
          psql_unescape(items[0]) == "SELECT 1; SELECT 2; SELECT 3;",
          f"{psql_unescape(items[0])!r}")

    # Plain semicolons still split.
    items = sql_items("SELECT 1; SELECT 2;\n")
    check("plain semis: two items", len(items) == 2, f"{items}")

    # Trailing `\;` with no final `;` still flushes as one item.
    items = sql_items("SELECT 1\\;\n")
    check("trailing escaped semi: one item", len(items) == 1, f"{items}")
    check("trailing escaped semi: unescapes",
          psql_unescape(items[0]) == "SELECT 1;", f"{psql_unescape(items[0])!r}")

    # `\;` inside a single-quoted string is literal: no split, no unescape.
    items = sql_items("SELECT 'a\\;b';\n")
    check("string: no split", len(items) == 1, f"{items}")
    check("string: backslash kept",
          psql_unescape(items[0]) == "SELECT 'a\\;b';", f"{psql_unescape(items[0])!r}")

    # `\;` inside a double-quoted identifier is literal too.
    items = sql_items('SELECT "a\\;b";\n')
    check("dquote: no split", len(items) == 1, f"{items}")
    check("dquote: backslash kept",
          psql_unescape(items[0]) == 'SELECT "a\\;b";', f"{psql_unescape(items[0])!r}")

    # `\;` inside a line comment never reaches the server: the splitter
    # drops inter-statement comments (pre-existing behavior), and the
    # comment's `\;` must not split or join anything.
    items = sql_items("SELECT 1; -- x\\;y\nSELECT 2;\n")
    check("comment: two items", len(items) == 2, f"{items}")
    check("comment: comment dropped, no stray split",
          items == ["SELECT 1;", "SELECT 2;"], f"{items}")

    # `\;` inside dollar quotes is literal.
    items = sql_items("SELECT $$a\\;b$$;\n")
    check("dollar: no split", len(items) == 1, f"{items}")
    check("dollar: backslash kept",
          psql_unescape(items[0]) == "SELECT $$a\\;b$$;", f"{psql_unescape(items[0])!r}")

    # Multi-line `\;` join, like transactions.sql.
    items = sql_items("SELECT 1\\;\nSELECT 2\\;\nSELECT 3;\n")
    check("multiline: single item", len(items) == 1, f"{items}")
    check("multiline: unescapes",
          psql_unescape(items[0]) == "SELECT 1;\nSELECT 2;\nSELECT 3;",
          f"{psql_unescape(items[0])!r}")

    # v1.04 regression: consecutive "--" comment lines must not parse as
    # a table (header "--" + dashes "--"); the second parse must stop.
    out = [
        'SELECT 1 AS "False";',
        ' False ',
        '-------',
        ' f',
        '(1 row)',
        '',
        '--',
        '--',
        '--',
        'CREATE TABLE NAME_TBL(f1 name);',
    ]
    blocks, new_pos = parse_expected_blocks(out, 1, "")
    check("comment-lines: one block", len(blocks) == 1, f"{len(blocks)}")
    check("comment-lines: it is the table",
          blocks and blocks[0].kind == "table", f"{[b.kind for b in blocks]}")
    check("comment-lines: pos stops before comments", new_pos == 6, f"{new_pos}")

    # Two genuine blocks still parse (multi-statement Query).
    out2 = [
        'SELECT 1\\; SELECT 2;',
        ' ?column?',
        '----------',
        '        1',
        '(1 row)',
        '',
        ' ?column?',
        '----------',
        '        2',
        '(1 row)',
    ]
    blocks2, _ = parse_expected_blocks(out2, 1, "")
    check("two tables: two blocks", len(blocks2) == 2, f"{len(blocks2)}")
    check("two tables: rows in order",
          [b.rows for b in blocks2] == [[["1"]], [["2"]]],
          f"{[b.rows for b in blocks2]}")

    print(f"\n{PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
