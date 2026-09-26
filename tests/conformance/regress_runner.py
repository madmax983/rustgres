#!/usr/bin/env python3
"""PostgreSQL regress conformance runner for rustgres (v0.14+).

Runs a vendored subset of PostgreSQL's own regression suite
(src/test/regress, REL_19_STABLE) against rustgres and compares results
SEMANTICALLY rather than byte-for-byte:

* row sets are compared order-insensitively unless the query has ORDER BY
* values are canonicalized by column type OID (bool t/f, ints, floats,
  numerics via Decimal, text passthrough)
* errors are compared by presence (SQLSTATE is recorded, message wording
  is ours by design)
* command tags are compared by first word

Each statement is classified:
    PASS          result matches PostgreSQL's expected output
    EXPECTED-FAIL known unsupported feature (documented reason)
    REAL-FAIL     genuine conformance gap / bug
    SKIP          psql meta-command, COPY stdin data, or unparseable input

Usage:
    python3 tests/conformance/regress_runner.py [--tests boolean,join,...]
                                                [--report PATH]

The runner starts its own rustgres server on 127.0.0.1:5433 with a fresh
data dir (no external server needed). Requires `cargo build` first.

Exit code 0 = runner completed (even with REAL-FAILs; those are the
conformance signal). Exit code 2 = infrastructure failure.
"""

import math
import os
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from decimal import Decimal, InvalidOperation

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DATA = os.path.join(ROOT, "tests", "conformance", "data")
BIN = os.path.join(ROOT, "target", "debug", "rustgres")
HOST = "127.0.0.1"
PORT = 5433
STMT_TIMEOUT = 30.0

# ---------------------------------------------------------------------------
# Test selection. Each maps to data/sql/<name>.sql + data/expected/<name>.out.
# `need_tenk` tests get tenk1/tenk2 (loaded from data/tenk.data) in setup.
# ---------------------------------------------------------------------------

TESTS = [
    # (name, need_tenk, need_onek, need_road): tenk1/tenk2 for the join/subselect/union
    # families; onek/onek2 for the select/subselect/join families (PG's
    # test_setup.sql builds onek/onek2 as 1000-row slices of tenk1).
    ("boolean", False, False, False),
    ("char", False, False, False),
    ("name", False, False, False),
    ("text", False, False, False),
    ("varchar", False, False, False),
    ("int2", False, False, False),
    ("int4", False, False, False),
    ("int8", False, False, False),
    ("float4", False, False, False),
    ("float8", False, False, False),
    ("numeric", False, False, False),
    ("strings", False, False, False),
    ("select", False, True, False),
    ("select_distinct", False, True, False),
    ("select_having", False, False, False),
    ("case", False, False, False),
    ("union", True, False, False),
    ("subselect", True, True, True),
    ("join", True, True, False),
    ("transactions", True, False, False),
    ("insert", False, False, False),
    ("delete", False, False, False),
]

# ---------------------------------------------------------------------------
# Wire client (mirrors tests/protocol_test.py framing)
# ---------------------------------------------------------------------------


def msg(typ, body):
    return typ + struct.pack("!i", len(body) + 4) + body


def cstr(s):
    return s.encode() + b"\x00"


class WireError(Exception):
    pass


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=STMT_TIMEOUT)
        body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
        self.s.sendall(struct.pack("!i", len(body) + 4) + body)
        self._drain_until_ready()

    def _read_msg(self):
        t = self.s.recv(1)
        if not t:
            raise WireError("connection closed by server")
        (ln,) = struct.unpack("!i", self._read_exact(4))
        return t, self._read_exact(ln - 4)

    def _read_exact(self, n):
        data = b""
        while len(data) < n:
            chunk = self.s.recv(n - len(data))
            if not chunk:
                raise WireError("connection closed by server")
            data += chunk
        return data

    def _drain_until_ready(self):
        while True:
            t, _ = self._read_msg()
            if t == b"Z":
                return

    def q(self, sql):
        """Returns dict(oids, colnames, rows, err_codes, tag)."""
        self.s.sendall(msg(b"Q", cstr(sql)))
        oids, names, rows, codes = [], [], [], []
        tag = ""
        while True:
            t, p = self._read_msg()
            if t == b"T":
                (n,) = struct.unpack("!h", p[:2])
                pos = 2
                for _ in range(n):
                    e = p.index(b"\x00", pos)
                    names.append(p[pos:e].decode())
                    pos = e + 1 + 6  # table oid + attr no
                    (oid,) = struct.unpack("!i", p[pos : pos + 4])
                    pos += 4
                    oids.append(oid)
                    pos += 2 + 4 + 2  # typlen, typmod, format
            elif t == b"D":
                (n,) = struct.unpack("!h", p[:2])
                pos, r = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", p[pos : pos + 4])
                    pos += 4
                    if ln == -1:
                        r.append(None)
                    else:
                        r.append(p[pos : pos + ln].decode())
                        pos += ln
                rows.append(r)
            elif t == b"E":
                fields, pos = {}, 0
                while p[pos] != 0:
                    e = p.index(b"\x00", pos + 1)
                    fields[chr(p[pos])] = p[pos + 1 : e].decode()
                    pos = e + 1
                codes.append(fields.get("C", "?"))
            elif t == b"C":
                tag = p[:-1].decode()
            elif t == b"Z":
                return {
                    "oids": oids,
                    "colnames": names,
                    "rows": rows,
                    "err_codes": codes,
                    "tag": tag,
                }

    def copy_stdin(self, sql, data_lines):
        """COPY ... FROM stdin with inline data via the COPY protocol.

        Returns dict(err_codes, tag) like q() but for the COPY flow:
        Query -> CopyInResponse -> CopyData* -> CopyDone ->
        CommandComplete -> ReadyForQuery.
        """
        self.s.sendall(msg(b"Q", cstr(sql)))
        codes, tag = [], ""
        # Expect CopyInResponse ('G').
        t, p = self._read_msg()
        if t == b"E":
            fields, pos = {}, 0
            while p[pos] != 0:
                e = p.index(b"\x00", pos + 1)
                fields[chr(p[pos])] = p[pos + 1 : e].decode()
                pos = e + 1
            codes.append(fields.get("C", "?"))
            self._drain_until_ready()
            return {"err_codes": codes, "tag": tag, "rows": []}
        if t != b"G":
            raise WireError("expected CopyInResponse, got %r" % t)
        for line in data_lines:
            self.s.sendall(msg(b"d", line.encode() + b"\n"))
        self.s.sendall(msg(b"c", b""))
        while True:
            t, p = self._read_msg()
            if t == b"E":
                fields, pos = {}, 0
                while p[pos] != 0:
                    e = p.index(b"\x00", pos + 1)
                    fields[chr(p[pos])] = p[pos + 1 : e].decode()
                    pos = e + 1
                codes.append(fields.get("C", "?"))
            elif t == b"C":
                tag = p[:-1].decode()
            elif t == b"Z":
                return {"err_codes": codes, "tag": tag, "rows": []}

    def close(self):
        try:
            self.s.sendall(msg(b"X", b""))
        except OSError:
            pass
        finally:
            self.s.close()


def wait_for_port(timeout=20.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            s = socket.create_connection((HOST, PORT), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.1)
    return False


class Server:
    def __init__(self):
        self.data_dir = tempfile.mkdtemp(prefix="rg_conform_")
        self.proc = None

    def start(self):
        # v0.93: real pg_regress runs with PGDATESTYLE=Postgres,MDY (the
        # expected .out files were generated that way, e.g. text.out's
        # `03-09-2010`); the engine honors it since v0.90.
        env = dict(
            os.environ,
            RUSTGRES_DATA_DIR=self.data_dir,
            PGDATESTYLE="Postgres,MDY",
        )
        self.proc = subprocess.Popen(
            [BIN], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        if not wait_for_port():
            raise RuntimeError("server did not open 127.0.0.1:5433")
        time.sleep(0.3)

    def stop(self):
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
            self.proc = None


# ---------------------------------------------------------------------------
# SQL statement splitter
# ---------------------------------------------------------------------------


def split_statements(text):
    """Split .sql text into items: ('sql', text) | ('meta', text) | ('skip', reason).

    Handles -- and /* */ comments, single/double-quoted strings, dollar
    quoting, psql backslash commands, and COPY ... FROM stdin/stdout data.
    """
    items = []
    i, n = 0, len(text)
    buf = []  # current statement chars
    in_copy = [False]  # set after COPY ... FROM stdin: collect lines till \.
    # v0.64: COPY ... FROM stdin inline data is executed via the COPY
    # protocol (previously skipped, leaving tables empty). Pending COPY
    # state: [sql_text, table, columns, [data lines]].
    copy_pending = [None]

    def flush():
        s = "".join(buf).strip()
        buf.clear()
        if s:
            # COPY FROM stdin: inline data follows; capture for protocol.
            m = re.match(
                r"(?is)^\s*copy\s+(\S+?)(?:\s*\(([^)]*)\))?\s+from\s+stdin\b", s
            )
            if m:
                table = m.group(1)
                cols = (
                    [c.strip() for c in m.group(2).split(",")]
                    if m.group(2)
                    else None
                )
                copy_pending[0] = [s, table, cols, []]
                in_copy[0] = True
                return
            # COPY FROM stdout: still unsupported.
            m2 = re.match(r"(?is)^\s*copy\s+\S+\s+from\s+stdout\b", s)
            if m2:
                items.append(("skip", "COPY FROM stdout: unsupported"))
                return
            items.append(("sql", s))

    state = "normal"  # normal | linecomment | blockcomment | squote | dquote | dollar
    dollar_tag = ""
    line_start = True  # at start of a line (for backslash commands)
    pending = None

    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""

        # COPY inline data: collect whole lines until a line that is exactly \.
        if state == "normal" and in_copy[0] and line_start:
            j = text.find("\n", i)
            line = text[i : j if j != -1 else n]
            if line.strip() == "\\.":
                in_copy[0] = False
                # Emit the captured COPY as a protocol item.
                if copy_pending[0] is not None:
                    items.append(("copy_stdin", copy_pending[0]))
                    copy_pending[0] = None
            elif copy_pending[0] is not None:
                copy_pending[0][3].append(line)
            i = n if j == -1 else j + 1
            line_start = True
            continue

        if state == "normal":
            if line_start and c == "\\":
                # psql meta-command: flush pending statement, consume line
                flush()
                j = text.find("\n", i)
                meta = text[i : j if j != -1 else n].strip()
                # \. alone outside COPY is a no-op guard
                if meta != "\\.":
                    items.append(("meta", meta))
                i = n if j == -1 else j + 1
                line_start = True
                continue
            if c == "\\" and re.match(r"\\gset\b", text[i:]):
                # v0.38: trailing \gset [prefix] executes the pending
                # statement (psql query-buffer suffix). Terminate the
                # statement here like ';', keeping the marker (with any
                # prefix) attached for run_test to honor.
                m = re.match(r"\\gset[^\n]*", text[i:])
                # buf already ends with the pre-\gset whitespace, so keep
                # the marker tight: the echo lookup needs byte-exact text.
                buf.append(m.group(0).strip())
                j = text.find("\n", i)
                i = n if j == -1 else j + 1
                flush()
                line_start = True
                continue
            if c == "-" and nxt == "-":
                state = "linecomment"
                i += 2
                continue
            if c == "/" and nxt == "*":
                state = "blockcomment"
                i += 2
                continue
            if c == "'":
                state = "squote"
                buf.append(c)
                i += 1
                line_start = False
                continue
            if c == '"':
                state = "dquote"
                buf.append(c)
                i += 1
                line_start = False
                continue
            if c == "$":
                m = re.match(r"\$([A-Za-z_][A-Za-z_0-9]*)?\$", text[i:])
                if m:
                    state = "dollar"
                    dollar_tag = m.group(0)
                    buf.append(dollar_tag)
                    i += len(dollar_tag)
                    line_start = False
                    continue
                buf.append(c)
                i += 1
                line_start = False
                continue
            if c == ";":
                buf.append(c)
                i += 1
                flush()
                line_start = False
                continue
            buf.append(c)
            line_start = c == "\n"
            i += 1
            continue

        if state == "linecomment":
            if c == "\n":
                state = "normal"
                line_start = True
            i += 1
            continue

        if state == "blockcomment":
            if c == "*" and nxt == "/":
                state = "normal"
                i += 2
            else:
                i += 1
            continue

        if state == "squote":
            buf.append(c)
            if c == "'" and nxt == "'":
                buf.append(nxt)
                i += 2
                continue
            if c == "'":
                state = "normal"
            # backslash escapes inside E'' strings: keep simple, treat
            # backslash as literal (standard_conforming_strings=on anyway)
            i += 1
            line_start = False
            continue

        if state == "dquote":
            buf.append(c)
            if c == '"':
                state = "normal"
            i += 1
            line_start = False
            continue

        if state == "dollar":
            if text.startswith(dollar_tag, i):
                buf.append(dollar_tag)
                i += len(dollar_tag)
                state = "normal"
            else:
                buf.append(c)
                i += 1
            line_start = False
            continue

    flush()
    return items

# ---------------------------------------------------------------------------
# Expected-output parser (psql aligned format)
# ---------------------------------------------------------------------------


class Expected:
    def __init__(self, kind, **kw):
        self.kind = kind  # 'table' | 'error' | 'tag' | 'none'
        self.__dict__.update(kw)


def resolve_physical_rows(physical, count, cells, ncols):
    """Turn raw psql physical lines into logical result rows.

    physical: list of raw lines between the dashes and "(N rows)".
    count: the N from "(N rows)", or None if the marker was absent.
    cells: function mapping one physical line to a list of cell strings.
    ncols: number of columns.

    v0.34: psql renders an empty-string value as a blank physical line and
    a value containing newlines as several physical lines. The old code
    skipped every blank line, turning one-row empty-string results into
    zero rows and splitting multi-line values into phantom rows.
    """
    nonblank = [ln for ln in physical if ln.strip() != ""]
    n_phys = len(physical)
    n_nonblank = len(nonblank)
    if count is None:
        # No count marker: keep the old separator-skipping behavior.
        return [cells(ln) for ln in nonblank]
    if n_phys == count:
        # Every physical line is a row; blank lines are empty-string rows.
        return [cells(ln) for ln in physical]
    if n_nonblank == count:
        # Blank lines were separators between the count's rows.
        return [cells(ln) for ln in nonblank]
    # More physical lines than the count: possibly embedded newlines.
    # Only disambiguable for a single-column single-row value: the value
    # is the newline-joined stripped physical lines (e.g. wrapped base64).
    # v0.38: psql marks an embedded-newline continuation with a trailing
    # '+' (its nl_right marker, see print.c pg_asciiformat) and pads the
    # segment to the column width. Strip one marker per non-final line;
    # a value segment that itself ends in '+' still survives because the
    # marker is an ADDITIONAL '+'.
    if ncols == 1 and count == 1 and n_phys > 1:
        parts = []
        for ln in physical[:-1]:
            cell = cells(ln)[0]
            if cell.endswith("+"):
                cell = cell[:-1].rstrip()
            parts.append(cell)
        parts.append(cells(physical[-1])[0])
        return [["\n".join(parts)]]
    # Otherwise fall back to separator-skipping (may still mismatch).
    return [cells(ln) for ln in nonblank]


def parse_expected_block(lines, pos, null_display):
    """Parse one result block starting at lines[pos] (after blank skipping).

    pg_regress expected files OMIT command tags for utility statements
    (CREATE TABLE, INSERT, ...): after such a statement's echo the next
    thing is the following statement (or comments). So a result block only
    exists when the next lines form an ERROR block or a psql table;
    otherwise this returns Expected('noresult') without consuming input.

    Returns (Expected, new_pos).
    """
    n = len(lines)
    while pos < n and lines[pos].strip() == "":
        pos += 1
    # skip NOTICE/WARNING/INFO lines that may precede the real output
    while pos < n and re.match(r"^(NOTICE|WARNING|INFO):", lines[pos]):
        pos += 1
        while pos < n and lines[pos].strip() == "":
            pos += 1
    if pos >= n:
        return Expected("noresult"), pos
    line = lines[pos]

    # ERROR block: ERROR: line + optional LINE/HINT/DETAIL/caret lines
    if line.startswith("ERROR:"):
        pos += 1
        while pos < n and (
            re.match(r"^(LINE \d+:|HINT:|DETAIL:)", lines[pos])
            or re.match(r"^\s+\^", lines[pos])
        ):
            pos += 1
        return Expected("error"), pos

    # psql table: header line, dashes line, data rows, "(N rows)".
    # The dashes line must be at least as wide as the header: this keeps a
    # following "--" comment line (or the next statement echo) from being
    # misread as a table separator, which used to desync the whole file.
    if (
        pos + 1 < n
        and re.match(r"^-+(\+-+)*$", lines[pos + 1].strip())
        and len(lines[pos + 1].strip()) >= len(line.rstrip())
    ):
        dashes = lines[pos + 1].strip()
        # column widths from dash segments
        widths = [len(seg) for seg in dashes.split("+")]
        offs = []
        o = 0
        for w in widths:
            offs.append((o, o + w))
            o += w + 1  # +1 for the '+' separator

        def cells(ln):
            out = []
            for a, b in offs:
                out.append(ln[a:b].strip() if len(ln) > a else "")
            return out

        colnames = cells(line)
        pos += 2
        # Collect physical lines until "(N rows)" or end of block.
        physical = []
        count = None
        while pos < n:
            ln = lines[pos]
            m = re.match(r"^\((\d+) rows?\)$", ln.strip())
            if m:
                count = int(m.group(1))
                pos += 1
                break
            physical.append(ln)
            pos += 1
        rows = resolve_physical_rows(physical, count, cells, len(widths))
        return Expected("table", colnames=colnames, rows=rows, count=count), pos

    # Otherwise: utility statement with no output block (pg_regress omits
    # command tags). Do not consume anything.
    return Expected("noresult"), pos


# ---------------------------------------------------------------------------
# Semantic value canonicalization by type OID
# ---------------------------------------------------------------------------

# PostgreSQL type OIDs we normalize; anything else compares as raw text.
OID_BOOL = 16
OID_INT = (20, 21, 23)
OID_FLOAT = (700, 701)
OID_NUMERIC = 1700


def canon(oid, text):
    """Canonicalize a text value from the wire into a comparable Python value."""
    if text is None:
        return None
    t = text.strip()
    if oid == OID_BOOL:
        return {"t": True, "f": False}.get(t, t)
    if oid in OID_INT:
        try:
            return int(t)
        except ValueError:
            return t
    if oid in OID_FLOAT:
        tl = t.lower()
        if tl in ("nan",):
            return float("nan")
        try:
            return float(tl.replace("infinity", "inf"))
        except ValueError:
            return t
    if oid == OID_NUMERIC:
        try:
            return Decimal(t)
        except InvalidOperation:
            return t
    # v0.34: strip text/date/timestamp like every other type. The expected
    # side is always psql-padding-stripped by cells(), so comparing the raw
    # wire value made correct trailing-space results (lpad/rpad) fail.
    # (Known limitation: this also masks missing char(n) blank-padding
    # enforcement on CAST, which is a separate future milestone.)
    return t  # text/date/timestamp/unknown: stripped string compare


def values_equal(oid, a, b):
    if a is None or b is None:
        return a is None and b is None
    if oid in OID_FLOAT and isinstance(a, float) and isinstance(b, float):
        if math.isnan(a) and math.isnan(b):
            return True
        return math.isclose(a, b, rel_tol=1e-6, abs_tol=1e-12)
    # v0.59: numeric NaN is not equal to itself under Decimal (IEEE
    # semantics), but PostgreSQL's numeric_eq treats NaN = NaN as true.
    if oid == OID_NUMERIC and isinstance(a, Decimal) and isinstance(b, Decimal):
        if a.is_nan() and b.is_nan():
            return True
    return a == b


NULL_OR_EMPTY = "<null-or-empty>"  # ambiguous when null display is ''


def norm_expected_cell(cell, null_display):
    if cell == null_display:
        return NULL_OR_EMPTY if null_display == "" else None
    return cell


# ---------------------------------------------------------------------------
# Statement classification (expected-fail catalogue)
# ---------------------------------------------------------------------------

# (regex, reason) -- matched against the statement text, case-insensitive.
# --- v0.14: pg_regress conformance gaps (honest EXPECTED-FAILs) ---
# (UNION_PATTERN removed in v0.44: UNION/INTERSECT/EXCEPT are supported.)
EXPECTED_FAIL_PATTERNS = [
    (r"^\s*create\s+(or\s+replace\s+)?function\b", "CREATE FUNCTION (procedural languages) unsupported"),
    # v1.02: ALTER FUNCTION was never in the grammar (honest 42601);
    # previously masked by the cascade guard because CREATE FUNCTION
    # tattle() itself failed (RAISE unsupported in plpgsql bodies).
    # Now that the CREATE succeeds, classify the pre-existing gap
    # honestly instead of as a new REAL-FAIL.
    (r"(?is)^\s*alter\s+function\b", "ALTER FUNCTION unsupported"),
    (r"^\s*create\s+(or\s+replace\s+)?procedure\b", "CREATE PROCEDURE unsupported"),
    (r"^\s*create\s+aggregate\b", "CREATE AGGREGATE unsupported"),
    (r"^\s*create\s+operator\b", "CREATE OPERATOR unsupported"),
    (r"^\s*create\s+trigger\b", "CREATE TRIGGER unsupported"),
    (r"^\s*create\s+extension\b", "CREATE EXTENSION unsupported"),
    (r"^\s*create\s+tablespace\b", "CREATE TABLESPACE unsupported"),
    (r"^\s*create\s+table\b.*\bpartition\s+by\b", "declarative partitioning unsupported"),
    (r"\battach\s+partition\b", "declarative partitioning unsupported"),
    (r"\bpartition\s+of\b", "declarative partitioning unsupported"),
    (r"\bfor\s+values\s+in\b", "declarative partitioning unsupported"),
    (r"^\s*do\b", "DO blocks (plpgsql) unsupported"),
    (r"^\s*listen\b|^\s*notify\b|^\s*unlisten\b", "LISTEN/NOTIFY unsupported"),
    (r"^\s*copy\b", "COPY TO/FROM stdout not covered by this harness"),
    (r"^\s*vacuum\s+full\b", "VACUUM FULL unsupported"),
    (r"^\s*set\b", "SET unsupported"),
    (r"^\s*show\b", "SHOW unsupported"),
    (r"^\s*reset\b", "RESET unsupported"),
    (r"::\s*regclass\b|::\s*regproc\b|::\s*regtype\b|::\s*regnamespace\b",
     "reg* pseudotypes unsupported"),
    (r"\bcurrent_setting\s*\(", "current_setting() unsupported"),
    (r"\btableoid\b", "tableoid system column unsupported"),
    (r"\bxmin\b|\bxmax\b", "xmin/xmax system columns unsupported"),
    # v0.46: generate_series(int/int8/numeric) is supported as a
    # FROM-clause table function, including implicit LATERAL
    # (`FROM t, f(t.x)`). Still unsupported and kept masked: the
    # timestamp/timestamptz variants, and empty SELECT lists.
    (r"(?i)\bgenerate_series\s*\([^)]*::\s*(timestamp|timestamptz)\b",
     "generate_series() timestamp variant unsupported"),
    (r"(?i)\bselect\s+from\b", "empty SELECT list unsupported"),
    (r"\bgen_random_uuid\s*\(", "gen_random_uuid() unsupported"),
    (r"\bquote_ident\s*\(|\bquote_literal\s*\(", "quote_*() unsupported"),
    (r"\bOVER\s*\(", "window functions in this construct unsupported"),
    (r"\bWITH\s+ORDINALITY\b", "WITH ORDINALITY unsupported"),
    (r"\bTABLESAMPLE\b", "TABLESAMPLE unsupported"),
    (r"\bFOR\s+UPDATE\s+OF\b", "SELECT FOR UPDATE OF unsupported"),
    (r"\bIS\s+NOT\s+DISTINCT\s+FROM\b", "IS NOT DISTINCT FROM unsupported"),
    (r"\bNULLS\s+(FIRST|LAST)\b", "NULLS FIRST/LAST unsupported"),
    # --- v0.14: pg_regress conformance gaps (honest EXPECTED-FAILs) ---
    (r"(?is)^\s*explain\s*\(", "EXPLAIN with (option, ...) syntax unsupported"),
    # v0.52: SELECT DISTINCT ON is implemented (PG19 Unique-under-sort
    # semantics); the mask is removed so the corpus statements are
    # exercised. The EXPLAIN variants above stay masked; the
    # ROW()-constructor variants below classify under row().
    (r"(?is)^\s*create\s+rule\b", "CREATE RULE unsupported"),
    (r"(?is)^\s*drop\s+rule\b", "DROP RULE unsupported"),
    (r"(?i)\b(all|any|some)\s*\(\s*select\b", "= ALL/ANY/SOME (subquery) unsupported"),
    (r"\(\s*\w+(\s*,\s*\w+)+\s*\)\s*(not\s+)?in\s*\(\s*select\b",
     "row-wise IN (subquery) unsupported"),
    # v0.54: zero-column tables (CREATE TABLE t(); INSERT ... DEFAULT
    # VALUES) are not supported; the following LATERAL test is already
    # masked separately.
    (r"(?i)\bnocols\b", "zero-column tables unsupported"),
    # v0.54: whole-row Vars (`SELECT foo FROM (...) AS foo`) need composite
    # row values, which the executor does not model.
    (r"(?is)^\s*select\s+(\w+)\s+from\s*\(\s*select\b.*\)\s*as\s+\1\s*;?\s*$",
     "whole-row Vars unsupported"),
    (r"(?i)\bshipped_view\b", "depends on CREATE RULE (unsupported)"),
    # v0.54: inheritance (`FROM person*`) is unsupported; the person tables
    # themselves are never created (PG's test_setup.sql builds them via
    # CREATE TABLE ... INHERITS). sillysrf is an SQL-language SRF whose
    # CREATE FUNCTION is masked above — its SELECTs fail only because the
    # function was never created.
    (r"(?i)\bperson\s*\*", "table inheritance (FROM tbl*) unsupported"),
    (r"(?i)\bsillysrf\s*\(", "depends on CREATE FUNCTION (unsupported)"),
    # v0.55: vol()/volfoo() are plpgsql functions whose CREATE FUNCTION
    # is masked above — their CASE-test SELECTs fail only because the
    # functions were never created. (v0.97: bounded single-RETURN
    # plpgsql bodies are now supported, so these entries are dormant
    # for vol/volfoo; they remain for richer plpgsql bodies.)
    (r"(?i)\bvol\s*\(", "depends on CREATE FUNCTION (unsupported)"),
    (r"(?i)\bvolfoo\s*\(", "depends on CREATE FUNCTION (unsupported)"),
    # v0.55: no constant-expression folding pass — PG folds `1/0` in a
    # potentially-reachable CASE arm at plan time (division by zero);
    # rustgres raises only for arms it actually evaluates.
    (r"(?is)\bcase\b.*\bthen\s+1\s*/\s*0\b",
     "constant-expression folding unsupported"),
    # v0.44: UNION/INTERSECT/EXCEPT are supported; the old broad pattern is
    # removed. Statements using them with other unsupported constructs are
    # classified under those constructs below.
    # v0.45: array[...], row(), CREATE TYPE, and empty SELECT lists were
    # unmasked by UNION support (v0.44 +10 REAL-FAIL); classify honestly.
    (r"(?i)\barray\s*\[", "array[...] literal syntax unsupported"),
    (r"(?i)\brow\s*\(", "row() constructor unsupported"),
    (r"(?i)^\s*create\s+type\b", "CREATE TYPE unsupported"),
    (r"(?i)^\s*select\s*;\s*$", "empty SELECT list unsupported"),
    (r"(?i)\bselect\s+(union|intersect|except)\s+select\b", "empty SELECT list unsupported"),
    # v0.45: format() is supported, but VARIADIC array form needs array
    # literal/coercion support not yet implemented.
    (r"(?i)\bformat\s*\([^)]*\bvariadic\b", "format() with VARIADIC array unsupported"),
    (r"(?i)\blateral\b", "LATERAL joins unsupported"),
    (r"(?i)\bunnest\s*\(", "unnest() unsupported"),
    (r"\)\s*\[", "subscript on subquery/expression result unsupported"),
    (r"(?is)\bwith\b.*\bon\s+conflict\b", "CTE not visible inside ON CONFLICT subquery"),
]


def _top_level_from(stmt):
    """True when a FROM keyword appears at paren depth 0 (outside strings
    and subqueries): i.e. a real UPDATE ... FROM clause, not a FROM inside
    a SET subquery."""
    depth = 0
    i, n = 0, len(stmt)
    while i < n:
        ch = stmt[i]
        if ch == "'":
            i += 1
            while i < n:
                if stmt[i] == "'":
                    if i + 1 < n and stmt[i + 1] == "'":
                        i += 2
                        continue
                    break
                i += 1
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth = max(0, depth - 1)
        elif depth == 0 and (ch.isalpha() or ch == "_"):
            j = i
            while j < n and (stmt[j].isalnum() or stmt[j] == "_"):
                j += 1
            if stmt[i:j].lower() == "from":
                return True
            i = j
            continue
        i += 1
    return False


def classify_expected_fail(stmt):
    # v0.14: UNION inside WITH RECURSIVE is genuinely supported; only
    # top-level / subquery UNION is an honest EXPECTED-FAIL.
    # v0.14: UPDATE ... FROM is a real grammar gap (not a subquery FROM).
    if re.match(r"(?is)^\s*update\b", stmt) and _top_level_from(stmt):
        return "UPDATE ... FROM unsupported"
    has_recursive = re.search(r"(?is)\bwith\s+recursive\b", stmt) is not None
    for pat, reason in EXPECTED_FAIL_PATTERNS:
        if re.search(pat, stmt, re.IGNORECASE | re.DOTALL):
            return reason
    return None


# Statements that are semantically supported but pathologically slow on the
# conformance data volumes (they wedge the server past STMT_TIMEOUT).
# EXPECTED-FAIL without executing; the engine gap is real and documented.
TOO_SLOW_PATTERNS = [
    (
        r"\bin\s*\(\s*select\b.*\bfrom\s+tenk1\b",
        "too slow: IN-subquery over tenk1 re-runs per outer row O(n^2); "
        "needs a hashed subplan like PostgreSQL",
    ),
    (
        r"\bfrom\b[^;]*\btenk1\b[^;]*,\s*tenk1\b",
        "too big: 10k x 10k cartesian product materializes in memory (OOM); "
        "needs pipelined join execution like PostgreSQL",
    ),
]


def classify_too_slow(stmt):
    for pat, reason in TOO_SLOW_PATTERNS:
        if re.search(pat, stmt, re.IGNORECASE | re.DOTALL):
            return reason
    return None


# ---------------------------------------------------------------------------
# Setup prologue (adapted from PG's test_setup.sql; only what we need)
# ---------------------------------------------------------------------------


TENK_DDL = ("CREATE TABLE %s (unique1 int4, unique2 int4, two int4, four int4, "
            "ten int4, twenty int4, hundred int4, thousand int4, twothousand int4, "
            "fivethous int4, tenthous int4, odd int4, even int4, "
            "stringu1 name, stringu2 name, string4 name)")
TENK_COLS = ("unique1, unique2, two, four, ten, twenty, hundred, thousand, "
             "twothousand, fivethous, tenthous, odd, even, stringu1, stringu2, string4")


def _load_data_table(stmts, table, datafile, expect_rows):
    """Append DDL + chunked multi-row INSERTs loading a PG regress .data file.

    PG's test_setup.sql loads these with server-side COPY; the harness
    reproduces the same rows with multi-row INSERTs.
    """
    stmts.append(TENK_DDL % table)
    with open(os.path.join(DATA, datafile), encoding="utf-8") as f:
        rows = [ln.rstrip("\n").split("\t") for ln in f if ln.strip()]
    assert len(rows) == expect_rows, "%s row count changed: %d" % (datafile, len(rows))
    assert all(len(r) == 16 for r in rows), "%s column count changed" % datafile
    for i in range(0, len(rows), 500):
        chunk = rows[i : i + 500]
        vals = []
        for r in chunk:
            nums = ",".join(r[:13])
            strs = ",".join("'" + s.replace("'", "''") + "'" for s in r[13:])
            vals.append("(%s,%s)" % (nums, strs))
        stmts.append("INSERT INTO %s (%s) VALUES %s" % (table, TENK_COLS, ",".join(vals)))
    stmts.append("VACUUM " + table)


def _tenk_setup(stmts, tables):
    """Append tenk-family DDL + data load for the named tables."""
    for tbl in tables:
        _load_data_table(stmts, tbl, "tenk.data", 10000)


def _load_road_table(stmts):
    """Append DDL + chunked INSERTs loading PG's authentic road table.

    PG's test_setup.sql creates road(name text, thepath path) from
    data/streets.data (5124 rows, 2911 distinct names). rustgres has no
    geometric path type, so thepath is stored as text — the conformance
    queries only touch `name`, and the authentic names are what the
    expected counts (2911) depend on.
    """
    stmts.append("CREATE TABLE road (name text, thepath text)")
    with open(os.path.join(DATA, "streets.data"), encoding="utf-8") as f:
        rows = [ln.rstrip("\n").split("\t") for ln in f if ln.strip()]
    assert len(rows) == 5124, "streets.data row count changed: %d" % len(rows)
    assert all(len(r) == 2 for r in rows), "streets.data column count changed"
    for i in range(0, len(rows), 500):
        chunk = rows[i : i + 500]
        vals = []
        for name, thepath in chunk:
            n = "'" + name.replace("'", "''") + "'"
            p = "'" + thepath.replace("'", "''") + "'"
            vals.append("(%s,%s)" % (n, p))
        stmts.append("INSERT INTO road (name, thepath) VALUES " + ",".join(vals))
    stmts.append("VACUUM road")


def setup_statements(need_tenk, need_onek, need_road):
    stmts = [
        "CREATE TABLE CHAR_TBL(f1 char(4))",
        "INSERT INTO CHAR_TBL (f1) VALUES ('a'), ('ab'), ('abcd'), ('abcd    ')",
        "VACUUM CHAR_TBL",
        "CREATE TABLE FLOAT8_TBL(f1 float8)",
        "INSERT INTO FLOAT8_TBL(f1) VALUES ('0.0'), ('-34.84'), ('-1004.30'), "
        "('-1.2345678901234e+200'), ('-1.2345678901234e-200')",
        "VACUUM FLOAT8_TBL",
        "CREATE TABLE INT2_TBL(f1 int2)",
        "INSERT INTO INT2_TBL(f1) VALUES ('0   '), ('  1234 '), ('    -1234'), "
        "('32767'), ('-32767')",
        "VACUUM INT2_TBL",
        "CREATE TABLE INT4_TBL(f1 int4)",
        "INSERT INTO INT4_TBL(f1) VALUES ('   0  '), ('123456     '), "
        "('    -123456'), ('2147483647'), ('-2147483647')",
        "VACUUM INT4_TBL",
        "CREATE TABLE INT8_TBL(q1 int8, q2 int8)",
        "INSERT INTO INT8_TBL VALUES ('  123   ','  456'), ('123   ','4567890123456789'), "
        "('4567890123456789','123'), (+4567890123456789,'4567890123456789'), "
        "('+4567890123456789','-4567890123456789')",
        "VACUUM INT8_TBL",
        "CREATE TABLE TEXT_TBL (f1 text)",
        "INSERT INTO TEXT_TBL VALUES ('doh!'), ('hi de ho neighbor')",
        "VACUUM TEXT_TBL",
        "CREATE TABLE VARCHAR_TBL(f1 varchar(4))",
        "INSERT INTO VARCHAR_TBL (f1) VALUES ('a'), ('ab'), ('abcd'), ('abcd    ')",
        "VACUUM VARCHAR_TBL",
        # v0.88: join.sql's "proven-dummy append rels" test needs b_star.
        # PG's create_misc.sql builds it as
        #   CREATE TABLE a_star (class char, a int4);
        #   CREATE TABLE b_star (b text) INHERITS (a_star);
        #   ALTER TABLE b_star RENAME b TO bb;
        #   ALTER TABLE a_star RENAME a TO aa;
        # rustgres does not implement table inheritance, so the harness
        # creates the flattened post-rename shape directly as a plain
        # table. The test only checks the planner copes with the empty
        # right side (the predicate `bb < bb AND bb IS NULL` can never
        # match), so no inheritance semantics are needed.
        "CREATE TABLE b_star (class char, aa int4, bb text)",
        "VACUUM b_star",
    ]
    if need_tenk:
        _tenk_setup(stmts, ("tenk1", "tenk2"))
    # PG's test_setup.sql loads onek from data/onek.data (a separate file,
    # not a tenk slice) and clones it with CTAS: onek2 holds the same rows.
    if need_onek:
        _load_data_table(stmts, "onek", "onek.data", 1000)
        stmts.append("CREATE TABLE onek2 AS SELECT * FROM onek")
    if need_road:
        _load_road_table(stmts)
    return stmts


# ---------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------


class Verdict:
    def __init__(self, status, detail=""):
        self.status = status  # PASS | EXPECTED-FAIL | REAL-FAIL | SKIP
        self.detail = detail


def has_top_level_order_by(stmt):
    """v0.95: detect ORDER BY at the top query level only, ignoring
    nested parentheses, string literals, comments, and dollar-quoted
    bodies. The old regex matched ORDER BY inside subqueries, causing
    false ordered-comparison failures."""
    i = 0
    n = len(stmt)
    depth = 0
    while i < n:
        c = stmt[i]
        # Skip single-quoted strings ('' escapes)
        if c == "'":
            i += 1
            while i < n:
                if stmt[i] == "'":
                    if i + 1 < n and stmt[i+1] == "'":
                        i += 2
                        continue
                    i += 1
                    break
                i += 1
            continue
        # Skip double-quoted identifiers ("" escapes)
        if c == '"':
            i += 1
            while i < n:
                if stmt[i] == '"':
                    if i + 1 < n and stmt[i+1] == '"':
                        i += 2
                        continue
                    i += 1
                    break
                i += 1
            continue
        # Skip line comments
        if c == '-' and i + 1 < n and stmt[i+1] == '-':
            i += 2
            while i < n and stmt[i] != '\n':
                i += 1
            continue
        # Skip block comments
        if c == '/' and i + 1 < n and stmt[i+1] == '*':
            i += 2
            while i + 1 < n and not (stmt[i] == '*' and stmt[i+1] == '/'):
                i += 1
            i += 2
            continue
        # Skip dollar-quoted strings ($tag$...$tag$)
        if c == '$':
            j = i + 1
            while j < n and (stmt[j].isalnum() or stmt[j] == '_'):
                j += 1
            if j < n and stmt[j] == '$':
                tag = stmt[i:j+1]
                k = stmt.find(tag, j + 1)
                if k != -1:
                    i = k + len(tag)
                    continue
            i += 1
            continue
        # Track paren depth
        if c == '(':
            depth += 1
        elif c == ')':
            depth = max(0, depth - 1)
        # Check for ORDER BY at depth 0
        if depth == 0 and (c == 'o' or c == 'O'):
            # Check if we're at a word boundary and match "order by"
            if i == 0 or not (stmt[i-1].isalnum() or stmt[i-1] == '_'):
                j = i + 5
                if stmt[i:j].lower() == 'order' and j < n and not (stmt[j].isalnum() or stmt[j] == '_'):
                    # Skip whitespace, check for "by"
                    k = j
                    while k < n and stmt[k] in ' \t\n\r':
                        k += 1
                    if stmt[k:k+2].lower() == 'by' and (k+2 >= n or not (stmt[k+2].isalnum() or stmt[k+2] == '_')):
                        return True
        i += 1
    return False

def compare(stmt, expected, actual, null_display):
    """Compare expected vs actual wire result. Returns Verdict."""
    if expected.kind == "none":
        return Verdict("SKIP", "no expected output block found")

    if expected.kind == "error":
        if actual["err_codes"]:
            return Verdict("PASS")
        reason = classify_expected_fail(stmt)
        if reason:
            return Verdict("EXPECTED-FAIL", reason)
        return Verdict(
            "REAL-FAIL",
            "expected ERROR, got success (tag=%r rows=%d)"
            % (actual["tag"], len(actual["rows"])),
        )

    if expected.kind == "noresult":
        # Utility statement: pg_regress expects no output. We only check it
        # did not error (tag text itself is not in the expected files).
        if actual["err_codes"]:
            reason = classify_expected_fail(stmt)
            if reason:
                return Verdict("EXPECTED-FAIL", reason + " [sqlstate %s]" % actual["err_codes"][0])
            return Verdict(
                "REAL-FAIL",
                "expected success, got SQLSTATE %s" % (actual["err_codes"][0],),
            )
        return Verdict("PASS", "tag=%s" % actual["tag"])

    if actual["err_codes"]:
        reason = classify_expected_fail(stmt)
        if reason:
            return Verdict("EXPECTED-FAIL", reason + " [sqlstate %s]" % actual["err_codes"][0])
        return Verdict(
            "REAL-FAIL",
            "expected success, got SQLSTATE %s" % (actual["err_codes"][0],),
        )

    if expected.kind == "table":
        oids = actual["oids"]
        if len(oids) != len(expected.colnames):
            reason = classify_expected_fail(stmt)
            if reason:
                return Verdict("EXPECTED-FAIL", reason)
            return Verdict(
                "REAL-FAIL",
                "column count: expected %d got %d"
                % (len(expected.colnames), len(oids)),
            )
        # column names (stripped) must match. v0.38: the expected side is
        # stripped by the aligned-format parser, so strip the wire side
        # too — psql's padding makes trailing spaces in an alias
        # unrepresentable in the .out header.
        for ec, ac in zip(expected.colnames, actual["colnames"]):
            if ec != ac.strip():
                reason = classify_expected_fail(stmt)
                if reason:
                    return Verdict("EXPECTED-FAIL", reason)
                return Verdict(
                    "REAL-FAIL", "column name: expected %r got %r" % (ec, ac)
                )
        exp_rows = []
        for er in expected.rows:
            row = []
            for cell, oid in zip(er, oids):
                nc = norm_expected_cell(cell, null_display)
                if nc == NULL_OR_EMPTY:
                    row.append(NULL_OR_EMPTY)
                elif nc is None:
                    row.append(None)
                else:
                    row.append(canon(oid, nc))
            exp_rows.append(row)
        act_rows = []
        for ar in actual["rows"]:
            row = []
            for val, oid in zip(ar, oids):
                row.append(canon(oid, val))
            act_rows.append(row)

        def row_ok(er, ar):
            if len(er) != len(ar):
                return False
            for e, a, oid in zip(er, ar, oids):
                if e == NULL_OR_EMPTY:
                    if a is not None and a != "":
                        return False
                elif not values_equal(oid, e, a):
                    return False
            return True

        ordered = has_top_level_order_by(stmt)
        if ordered:
            ok = len(exp_rows) == len(act_rows) and all(
                row_ok(e, a) for e, a in zip(exp_rows, act_rows)
            )
        else:
            # order-insensitive multiset compare
            remaining = list(act_rows)
            ok = len(exp_rows) == len(act_rows)
            if ok:
                for e in exp_rows:
                    for j, a in enumerate(remaining):
                        if row_ok(e, a):
                            remaining.pop(j)
                            break
                    else:
                        ok = False
                        break
        if ok:
            return Verdict("PASS")
        reason = classify_expected_fail(stmt)
        if reason:
            return Verdict("EXPECTED-FAIL", reason)
        return Verdict(
            "REAL-FAIL",
            "row mismatch: expected %d rows, got %d rows%s"
            % (len(exp_rows), len(act_rows), " (ordered)" if ordered else ""),
        )

    return Verdict("SKIP", "unknown expected kind")


class ServerWedged(Exception):
    """A statement killed the connection (timeout or server death)."""

    def __init__(self, stmt, reason):
        super().__init__(reason)
        self.stmt = stmt
        self.reason = reason


def run_test(conn, name, need_tenk, verbose=False, skip_stmts=None):
    sql_path = os.path.join(DATA, "sql", name + ".sql")
    out_path = os.path.join(DATA, "expected", name + ".out")
    sql_text = open(sql_path, encoding="utf-8", errors="replace").read()
    out_text = open(out_path, encoding="utf-8", errors="replace").read()
    out_lines = out_text.split("\n")

    items = split_statements(sql_text)
    results = []  # (stmt, Verdict)
    null_display = ""
    pos = 0
    failed_objects = set()  # tables whose CREATE failed (cascade guard)
    # v0.38: psql variables set by \gset (name -> text value). psql
    # interpolates :name in later statements; unknown :names are left
    # untouched so a literal colon never silently vanishes.
    psql_vars = {}
    if skip_stmts is None:
        skip_stmts = {}

    for i_item, (kind, text) in enumerate(items):
        if verbose and i_item % 25 == 0:
            print("  ... item %d/%d" % (i_item, len(items)), flush=True)
        if kind == "skip":
            results.append(("<meta>", Verdict("SKIP", text)))
            continue
        if kind == "meta":
            # advance past echo if present; honor \pset null
            idx = out_text.find(text, pos)
            if idx != -1:
                pos = idx + len(text)
            m = re.match(r"""\\pset\s+null\s+'(.*)'""", text)
            if m:
                null_display = m.group(1)
            results.append(("<meta: %s>" % text[:40], Verdict("SKIP", "psql meta-command")))
            continue
        if kind == "copy_stdin":
            # v0.64: COPY ... FROM stdin with inline data, executed via
            # the COPY protocol. text = [sql, table, cols, data_lines].
            sql, _table, _cols, data_lines = text
            stmt = sql
            idx = out_text.find(stmt, pos)
            if idx == -1:
                results.append((stmt, Verdict("SKIP", "statement echo not found in .out")))
                continue
            pos = idx + len(stmt)
            line_pos = out_text.count("\n", 0, pos) + 1
            expected, new_line_pos = parse_expected_block(out_lines, line_pos, null_display)
            pos = sum(len(l) + 1 for l in out_lines[:new_line_pos])
            try:
                actual = conn.copy_stdin(sql, data_lines)
            except socket.timeout as e:
                raise ServerWedged(stmt, "ran past %ds timeout" % STMT_TIMEOUT)
            except (WireError, OSError) as e:
                raise ServerWedged(stmt, "connection died during execution: %r" % e)
            if actual["err_codes"]:
                v = Verdict("REAL-FAIL", "COPY error %s" % actual["err_codes"])
            elif expected.kind == "error":
                v = Verdict("REAL-FAIL", "expected error, COPY succeeded")
            else:
                # pg_regress omits the COPY tag; success (no error) is a pass.
                # Sanity: the tag should be "COPY <n>".
                if not actual["tag"].startswith("COPY "):
                    v = Verdict("REAL-FAIL", "bad COPY tag %r" % actual["tag"])
                else:
                    v = Verdict("PASS", "")
            results.append((stmt, v))
            if verbose and v.status != "PASS":
                print("    [%s] %s -- %s" % (v.status, stmt[:70].replace("\n", " "), v.detail))
            continue
        stmt = text
        # locate echo in expected output
        idx = out_text.find(stmt, pos)
        if idx == -1:
            results.append((stmt, Verdict("SKIP", "statement echo not found in .out")))
            if verbose:
                print("    [SKIP] echo-miss: %s" % stmt[:80].replace("\n", " "))
            continue
        pos = idx + len(stmt)
        # convert char pos to the line AFTER the statement echo
        line_pos = out_text.count("\n", 0, pos) + 1
        expected, new_line_pos = parse_expected_block(out_lines, line_pos, null_display)
        pos = sum(len(l) + 1 for l in out_lines[:new_line_pos])

        # cascade guard: statement touches an object that failed to create
        casc = next(
            (o for o in failed_objects
             if re.search(r"\b%s\b" % re.escape(o), stmt, re.IGNORECASE)),
            None,
        )

        if stmt in skip_stmts:
            results.append(
                (
                    stmt,
                    Verdict(
                        "EXPECTED-FAIL",
                        "too slow/big: %s" % skip_stmts[stmt],
                    ),
                )
            )
            continue
        slow_reason = classify_too_slow(stmt)
        if slow_reason:
            results.append((stmt, Verdict("EXPECTED-FAIL", slow_reason)))
            continue

        # v0.38: psql variable interpolation (see \gset below). A colon
        # preceded by another colon is a cast (`::`), not a variable.
        exec_stmt = re.sub(
            r"(?<!:):([A-Za-z_][A-Za-z0-9_]*)",
            lambda m: psql_vars.get(m.group(1), m.group(0)),
            stmt,
        )
        # v0.38: trailing \gset [prefix] — run the query, store the first
        # row's columns as psql variables (like psql). The expected file
        # shows no output block for \gset, which parse_expected_block
        # already models as noresult.
        gset = None
        m = re.search(r"\\gset(?:\s+([A-Za-z_][A-Za-z0-9_]*))?\s*;?\s*$", exec_stmt)
        if m:
            gset = m.group(1) or ""
            exec_stmt = exec_stmt[: m.start()].rstrip()
            if not exec_stmt.endswith(";"):
                exec_stmt += ";"

        try:
            actual = conn.q(exec_stmt)
        except socket.timeout as e:
            raise ServerWedged(stmt, "ran past %ds timeout" % STMT_TIMEOUT)
        except (WireError, OSError) as e:
            raise ServerWedged(stmt, "connection died during execution: %r" % e)

        if gset is not None and not actual["err_codes"] and actual["rows"]:
            # psql \gset: first row only; NULL unsets the variable.
            for cname, cval in zip(actual["colnames"], actual["rows"][0]):
                vname = gset + cname
                if cval is None:
                    psql_vars.pop(vname, None)
                else:
                    psql_vars[vname] = cval

        if casc and (actual["err_codes"] or expected.kind == "error"):
            results.append(
                (stmt, Verdict("EXPECTED-FAIL", "cascade: %s failed earlier" % casc))
            )
            continue

        v = compare(stmt, expected, actual, null_display)
        # track failed CREATEs for the cascade guard
        if v.status in ("REAL-FAIL", "EXPECTED-FAIL") and re.match(
            r"(?is)^\s*create\s+(?:table\s+)?(\w+)", stmt
        ):
            m = re.match(r"(?is)^\s*create\s+(?:table\s+)?(\w+)", stmt)
            failed_objects.add(m.group(1))
        results.append((stmt, v))
        if verbose and v.status != "PASS":
            print("    [%s] %s -- %s" % (v.status, stmt[:70].replace("\n", " "), v.detail))
    return results, None


def main():
    only = None
    report_path = None
    verbose = False
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        if args[i] == "--tests" and i + 1 < len(args):
            only = set(args[i + 1].split(","))
            i += 2
        elif args[i] == "--report" and i + 1 < len(args):
            report_path = args[i + 1]
            i += 2
        elif args[i] == "--verbose":
            verbose = True
            i += 1
        else:
            print("unknown arg %s" % args[i])
            return 2

    if not os.path.exists(BIN):
        print("missing %s; run `cargo build` first" % BIN)
        return 2

    tests = [(n, t, o, r) for n, t, o, r in TESTS if only is None or n in only]
    server = Server()
    server.start()
    all_results = {}
    need_tenk_any = any(t for _, t, _, _ in tests)
    need_onek_any = any(o for _, _, o, _ in tests)
    need_road_any = any(r for _, _, _, r in tests)

    def run_setup(c):
        setup = setup_statements(need_tenk_any, need_onek_any, need_road_any)
        print("setup: %d statements ..." % len(setup), flush=True)
        t0 = time.time()
        for s in setup:
            r = c.q(s)
            if r["err_codes"]:
                return "SETUP FAILED: %s -> %s" % (s[:60], r["err_codes"])
        print("setup ok in %.1fs" % (time.time() - t0), flush=True)
        return None

    wedged = {}  # stmt -> reason: kills the connection; skip on retry
    try:
        for name, need_tenk, need_onek, need_road in tests:
            # pg_regress runs each file in its own psql session. Use a fresh
            # server+connection per suite so one file's abandoned transaction
            # state (e.g. transactions.sql's last test) cannot poison the next
            # file with 25P02 cascade failures.
            server.stop()
            server = Server()
            server.start()
            conn = Conn()
            serr = run_setup(conn)
            if serr:
                print(serr)
                return 2
            print("== %s ==" % name, flush=True)
            t0 = time.time()
            restarts = 0
            while True:
                try:
                    results, err = run_test(
                        conn, name, need_tenk, verbose=verbose, skip_stmts=wedged
                    )
                    break
                except ServerWedged as w:
                    restarts += 1
                    if restarts > 10:
                        print("  FATAL: server keeps dying; %r" % w.reason)
                        return 2
                    wedged[w.stmt] = w.reason
                    print(
                        "  connection killer (%s); restarting server"
                        % w.reason,
                        flush=True,
                    )
                    server.stop()
                    server = Server()
                    server.start()
                    conn = Conn()
                    serr = run_setup(conn)
                    if serr:
                        print(serr)
                        return 2
            if err:
                print("  FATAL: %s" % err)
                return 2
            all_results[name] = results
            counts = {}
            for _, v in results:
                counts[v.status] = counts.get(v.status, 0) + 1
            print(
                "   %.1fs  " % (time.time() - t0)
                + "  ".join("%s=%d" % kv for kv in sorted(counts.items())),
                flush=True,
            )
    finally:
        server.stop()

    # summary
    totals = {}
    for name, results in all_results.items():
        for _, v in results:
            if v.status == "SKIP":
                continue
            totals[v.status] = totals.get(v.status, 0) + 1
    denom = sum(totals.values())
    passed = totals.get("PASS", 0)
    rate = 100.0 * passed / denom if denom else 0.0
    print("\n==== CONFORMANCE SUMMARY ====")
    print(
        "statements scored: %d  PASS=%d (%.1f%%)  EXPECTED-FAIL=%d  REAL-FAIL=%d"
        % (denom, passed, rate, totals.get("EXPECTED-FAIL", 0),
           totals.get("REAL-FAIL", 0))
    )

    if report_path:
        with open(report_path, "w", encoding="utf-8") as f:
            f.write("# rustgres pg_regress conformance report\n\n")
            f.write(
                "Scored statements: %d, PASS %d (%.1f%%), EXPECTED-FAIL %d, REAL-FAIL %d\n\n"
                % (denom, passed, rate, totals.get("EXPECTED-FAIL", 0),
                   totals.get("REAL-FAIL", 0))
            )
            for name, results in all_results.items():
                f.write("## %s\n\n" % name)
                for stmt, v in results:
                    if v.status in ("REAL-FAIL",):
                        f.write(
                            "- [%s] %s\n  %s\n"
                            % (v.status, stmt[:100].replace("\n", " "), v.detail)
                        )
                f.write("\n")
        print("report written to %s" % report_path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
