#!/usr/bin/env python3
"""Full PostgreSQL 19 regression schedule against rustgres.

Runs every suite in tests/conformance/pg19/parallel_schedule (vendored by
vendor_pg19.sh) the way pg_regress does: ONE database, PG's real
test_setup.sql first, then each suite in schedule order in its own
session, so later suites see the objects earlier suites created.

This is the honest-coverage counterpart to regress_runner.py, which runs
a curated 22-suite subset with a hand-written setup and a list of
documented EXPECTED-FAIL masks. Differences:

* No legacy masks. Every mismatch is REAL-FAIL except the one category
  rustgres will never support: C-language functions loaded from PG's
  regress.so (and statements that fail only because such a function was
  never created). The TOO_SLOW masks still apply: they keep a 10k x 10k
  cartesian product from exhausting memory.
* Cascades are reported, not hidden. A failure on a statement that names
  an object whose CREATE failed earlier (in any suite) stays REAL-FAIL,
  with a `cascade:` detail so reports can separate root causes from
  knock-on failures.
* psql features the suites rely on are modeled: variable interpolation
  (:name, :'name', :"name", :{?name}), \\set / \\unset / \\getenv,
  \\if / \\elif / \\else / \\endif / \\quit, \\c reconnects, \\x expanded
  mode (executed, but output is unscored because the harness parses only
  aligned tables). `COPY t FROM '<srcdir>/data/x.data'` (server-side file
  read) is sent as COPY FROM STDIN with the vendored file's contents, so
  data loads work; those statements are unscored on success.

Comparison semantics are exactly regress_runner's (compare_multi): row
sets order-insensitive unless top-level ORDER BY, values canonicalized
by type OID, errors compared by presence.

Usage:
    python3 tests/conformance/schedule_runner.py [--tests a,b,c]
        [--report PATH.md] [--json PATH.json] [--release] [--verbose]

--tests restricts which suites are *scored*; test_setup always runs
first. Suites skipped by --tests are not run at all, so dependent suites
may fail more than in a full run.
"""

import importlib.util
import json
import os
import re
import resource
import socket
import subprocess
import sys
import time
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
PG19 = os.path.join(HERE, "pg19")

_spec = importlib.util.spec_from_file_location(
    "regress_runner", os.path.join(HERE, "regress_runner.py"))
rr = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rr)

# Suites that test the psql client itself (its meta-commands, \crosstabview,
# pipeline mode), not the server. Not run.
EXCLUDED = {
    "psql": "tests the psql client (meta-commands, \\d output formatting)",
    "psql_crosstab": "tests psql's \\crosstabview client feature",
    "psql_pipeline": "tests psql's client-side pipeline mode",
}

# Fake installation paths for \getenv. Data files under SRCDIR/data/ are
# mapped back to pg19/data/ for COPY FROM translation; LIBDIR paths mark
# regress.so C functions.
FAKE_ENV = {
    "PG_ABS_SRCDIR": "/__pg19__/src",
    "PG_ABS_BUILDDIR": "/__pg19__/build",
    "PG_LIBDIR": "/__pg19__/lib",
    "PG_DLSUFFIX": ".so",
}
C_FUNC_REASON = (
    "C-language function from PG's regress.so: loading native extension "
    "code is out of scope"
)
C_CASCADE_REASON = "depends on a regress.so C function (out of scope)"

# Per-process address-space cap for the server, so a runaway statement
# fails inside the server instead of exhausting the machine.
SERVER_AS_LIMIT = 8 * 1024 ** 3


def classify_schedule(stmt):
    """Replacement for rr.classify_expected_fail in schedule mode: only the
    out-of-scope C-function category is EXPECTED-FAIL."""
    if re.search(r"/__pg19__/lib/|\blanguage\s+'?c'?\b", stmt, re.IGNORECASE):
        return C_FUNC_REASON
    return None


# compare/compare_multi resolve this name from the module globals, so
# patching our private copy of the module leaves regress_runner.py itself
# (and its 22-suite gate) untouched.
rr.classify_expected_fail = classify_schedule


def schedule_tests():
    tests = []
    with open(os.path.join(PG19, "parallel_schedule")) as f:
        for line in f:
            if line.startswith("test:"):
                tests.extend(line[5:].split())
    return tests


# ---------------------------------------------------------------------------
# psql emulation
# ---------------------------------------------------------------------------


def quote_literal(v):
    if "\\" in v:
        return "E'" + v.replace("\\", "\\\\").replace("'", "''") + "'"
    return "'" + v.replace("'", "''") + "'"


def quote_ident(v):
    return '"' + v.replace('"', '""') + '"'


def interpolate(text, pvars):
    """psql variable interpolation outside quotes, comments and dollar
    bodies. Undefined variables are left verbatim, like psql."""
    out = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == "'" or c == '"':
            j = i + 1
            while j < n:
                if text[j] == c:
                    if j + 1 < n and text[j + 1] == c:
                        j += 2
                        continue
                    break
                j += 1
            out.append(text[i:j + 1])
            i = j + 1
            continue
        if c == "-" and text.startswith("--", i):
            j = text.find("\n", i)
            j = n if j == -1 else j
            out.append(text[i:j])
            i = j
            continue
        if c == "$":
            m = re.match(r"\$([A-Za-z_][A-Za-z_0-9]*)?\$", text[i:])
            if m:
                tag = m.group(0)
                j = text.find(tag, i + len(tag))
                j = n if j == -1 else j + len(tag)
                out.append(text[i:j])
                i = j
                continue
        if c == ":" and not (i > 0 and text[i - 1] == ":") and not text.startswith("::", i):
            m = re.match(r":'([A-Za-z_][A-Za-z0-9_]*)'", text[i:])
            if m and m.group(1) in pvars:
                out.append(quote_literal(pvars[m.group(1)]))
                i += len(m.group(0))
                continue
            m = re.match(r':"([A-Za-z_][A-Za-z0-9_]*)"', text[i:])
            if m and m.group(1) in pvars:
                out.append(quote_ident(pvars[m.group(1)]))
                i += len(m.group(0))
                continue
            m = re.match(r":\{\?([A-Za-z_][A-Za-z0-9_]*)\}", text[i:])
            if m:
                out.append("TRUE" if m.group(1) in pvars else "FALSE")
                i += len(m.group(0))
                continue
            m = re.match(r":([A-Za-z_][A-Za-z0-9_]*)", text[i:])
            if m and m.group(1) in pvars:
                out.append(pvars[m.group(1)])
                i += len(m.group(0))
                continue
        out.append(c)
        i += 1
    return "".join(out)


def meta_args(rest, pvars):
    """Split psql meta-command arguments: 'quoted' (with '' and backslash
    escapes), :var / :'var' / :"var" interpolation, bare words. Adjacent
    pieces without whitespace concatenate, like psql."""
    args = []
    cur = None
    i, n = 0, len(rest)
    while i < n:
        c = rest[i]
        if c.isspace():
            if cur is not None:
                args.append(cur)
                cur = None
            i += 1
            continue
        piece = None
        if c == "'":
            j, buf = i + 1, []
            while j < n:
                if rest[j] == "'" and j + 1 < n and rest[j + 1] == "'":
                    buf.append("'")
                    j += 2
                    continue
                if rest[j] == "'":
                    break
                if rest[j] == "\\" and j + 1 < n:
                    buf.append({"n": "\n", "t": "\t"}.get(rest[j + 1], rest[j + 1]))
                    j += 2
                    continue
                buf.append(rest[j])
                j += 1
            piece, i = "".join(buf), j + 1
        elif c == ":":
            m = (re.match(r":'([A-Za-z_]\w*)'", rest[i:])
                 or re.match(r':"([A-Za-z_]\w*)"', rest[i:])
                 or re.match(r":\{\?([A-Za-z_]\w*)\}", rest[i:])
                 or re.match(r":([A-Za-z_]\w*)", rest[i:]))
            if m:
                name = m.group(1)
                tok = m.group(0)
                if tok.startswith(":{?"):
                    piece = "TRUE" if name in pvars else "FALSE"
                elif name in pvars:
                    v = pvars[name]
                    piece = (quote_literal(v) if tok.startswith(":'")
                             else quote_ident(v) if tok.startswith(':"') else v)
                else:
                    piece = tok
                i += len(tok)
            else:
                piece, i = c, i + 1
        else:
            j = i
            while j < n and not rest[j].isspace() and rest[j] not in "':":
                j += 1
            piece, i = rest[i:j], j
        cur = piece if cur is None else cur + piece
    if cur is not None:
        args.append(cur)
    return args


def psql_bool(v):
    """psql ParseVariableBool; unrecognized values are false (psql warns)."""
    v = v.strip().lower()
    if not v:
        return False
    for word, val in (("true", True), ("false", False), ("yes", True), ("no", False)):
        if word.startswith(v):
            return val
    if len(v) >= 2 and "on".startswith(v):
        return True
    if len(v) >= 2 and "off".startswith(v):
        return False
    if v == "1":
        return True
    return False


CREATE_NAME = re.compile(
    r"(?is)^\s*create\s+(?:or\s+replace\s+)?"
    r"(?:(?:global|local)\s+)?(?:temp(?:orary)?\s+|unlogged\s+)?"
    r"(?:recursive\s+)?(?:materialized\s+)?(?:unique\s+)?"
    r"(table|view|index|sequence|type|domain|function|procedure|aggregate|"
    r"schema|role|user|operator\s+class|operator|trigger|rule|policy|cast)\s+"
    r"(?:concurrently\s+)?(?:if\s+not\s+exists\s+)?"
    r"(?:only\s+)?([A-Za-z_][\w.]*|\"[^\"]+\")"
)


def created_name(stmt):
    m = CREATE_NAME.match(stmt)
    if not m:
        return None
    name = m.group(2).strip('"').split(".")[-1]
    if name.lower() in ("on", "as", "if", "public"):
        return None
    return name


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------


def limit_server_memory():
    try:
        resource.setrlimit(resource.RLIMIT_AS, (SERVER_AS_LIMIT, SERVER_AS_LIMIT))
    except (ValueError, OSError):
        pass


class SharedServer(rr.Server):
    """A server whose data dir survives restarts (WAL recovery brings the
    committed state back), like a pg_regress cluster that crashed."""

    restarts = 0

    @property
    def log_path(self):
        return self.data_dir + ".server.log"

    def start(self):
        env = dict(os.environ, RUSTGRES_DATA_DIR=self.data_dir,
                   PGDATESTYLE="Postgres,MDY")
        # Server stderr (panics, recovery errors) goes to a log beside the
        # data dir; it is the first place to look after a restart.
        log = open(self.log_path, "ab")
        self.proc = subprocess.Popen(
            [rr.BIN], env=env, stdout=subprocess.DEVNULL,
            stderr=log, preexec_fn=limit_server_memory)
        log.close()
        end = time.time() + 120.0
        while time.time() < end:
            if self.proc.poll() is not None:
                raise ServerUnrecoverable(
                    "server exited with %s on startup (see %s)"
                    % (self.proc.returncode, self.log_path))
            if rr.wait_for_port(timeout=1.0):
                time.sleep(0.2)
                return
        raise ServerUnrecoverable("server did not open 127.0.0.1:%d" % rr.PORT)

    def restart(self):
        self.restarts += 1
        print("    ! server restart #%d (log: %s)" % (self.restarts, self.log_path),
              flush=True)
        self.stop()
        self.start()


class ServerUnrecoverable(Exception):
    """The server cannot be brought back (e.g. WAL recovery fails)."""


class Session:
    """One suite's psql session state."""

    def __init__(self, server, user="postgres"):
        self.server = server
        self.user = user
        self.conn = rr.Conn(user=user)
        self.pvars = dict(FAKE_VARS_BASE)
        self.null_display = ""
        self.expanded = False

    def reconnect(self, user=None):
        try:
            self.conn.close()
        except Exception:
            pass
        if user is not None:
            self.user = user
        self.conn = rr.Conn(user=self.user)

    def recover(self):
        """Server died or wedged: restart on the same data dir, reconnect."""
        self.server.restart()
        self.conn = rr.Conn(user=self.user)


# Variables psql defines at startup that suites test with :{?...} or use.
FAKE_VARS_BASE = {"VERBOSITY": "default", "SHOW_CONTEXT": "errors"}


def load_data_file(path):
    rel = path[len(FAKE_ENV["PG_ABS_SRCDIR"]):].lstrip("/")
    local = os.path.join(PG19, rel)
    if not os.path.isfile(local):
        return None
    with open(local, encoding="utf-8", errors="replace") as f:
        return f.read().split("\n")[:-1]


COPY_FROM_FILE = re.compile(
    r"(?is)^\s*copy\s+(\S+?)(\s*\([^)]*\))?\s+from\s+'(/__pg19__/src/[^']+)'(.*)$")


def run_suite(session, name, failed_objects, verbose=False):
    sql_text = open(os.path.join(PG19, "sql", name + ".sql"),
                    encoding="utf-8", errors="replace").read()
    out_text = open(os.path.join(PG19, "expected", name + ".out"),
                    encoding="utf-8", errors="replace").read()
    out_lines = out_text.split("\n")
    items = rr.split_statements(sql_text, keep_inner_comments=True)
    results = []  # (stmt, Verdict, err_msg)
    pos = 0
    # \if stack: each entry [branch_active, any_branch_taken, parent_active]
    if_stack = []

    def active():
        return all(e[0] for e in if_stack)

    def advance_echo(text):
        nonlocal pos
        idx = out_text.find(text, pos)
        if idx != -1:
            pos = idx + len(text)
        return idx

    def expected_after_echo(stmt):
        nonlocal pos
        idx = out_text.find(stmt, pos)
        if idx == -1:
            return None
        pos = idx + len(stmt)
        line_pos = out_text.count("\n", 0, pos) + 1
        blocks, new_line_pos = rr.parse_expected_blocks(
            out_lines, line_pos, session.null_display)
        pos = sum(len(l) + 1 for l in out_lines[:new_line_pos])
        return blocks

    def execute(fn, stmt):
        try:
            return fn(), None
        except socket.timeout:
            reason = "ran past %ds timeout" % rr.STMT_TIMEOUT
        except (rr.WireError, OSError) as e:
            reason = "connection died during execution: %r" % (e,)
        session.recover()
        return None, reason

    for kind, text in items:
        if kind == "skip":
            continue
        if kind == "meta":
            cmd, _, rest = text.partition(" ")
            cmd = cmd.strip()
            rest = rest.strip()
            # Conditionals are evaluated even inside inactive branches
            # (to keep the nesting straight), like psql.
            if cmd == "\\if":
                parent = active()
                val = parent and psql_bool(" ".join(meta_args(rest, session.pvars)))
                if_stack.append([val, val, parent])
                advance_echo(text)
                continue
            if cmd == "\\elif" and if_stack:
                top = if_stack[-1]
                if top[1] or not top[2]:
                    top[0] = False
                else:
                    top[0] = psql_bool(" ".join(meta_args(rest, session.pvars)))
                    top[1] = top[0]
                advance_echo(text)
                continue
            if cmd == "\\else" and if_stack:
                top = if_stack[-1]
                top[0] = top[2] and not top[1]
                top[1] = True
                advance_echo(text)
                continue
            if cmd == "\\endif" and if_stack:
                if_stack.pop()
                advance_echo(text)
                continue
            if not active():
                continue
            advance_echo(text)
            if cmd in ("\\q", "\\quit"):
                break
            if cmd == "\\set":
                args = meta_args(rest, session.pvars)
                if args:
                    session.pvars[args[0]] = "".join(args[1:])
            elif cmd == "\\unset":
                for a in meta_args(rest, session.pvars):
                    session.pvars.pop(a, None)
            elif cmd == "\\getenv":
                args = meta_args(rest, session.pvars)
                if len(args) == 2:
                    val = FAKE_ENV.get(args[1], os.environ.get(args[1]))
                    if val is None:
                        session.pvars.pop(args[0], None)
                    else:
                        session.pvars[args[0]] = val
            elif cmd in ("\\c", "\\connect"):
                args = meta_args(rest, session.pvars)
                user = args[1] if len(args) > 1 and args[1] != "-" else None
                try:
                    session.reconnect(user)
                except (rr.WireError, OSError):
                    # Login refused (e.g. role does not exist): psql keeps
                    # the previous connection.
                    session.reconnect(None)
            elif cmd == "\\pset":
                m = re.match(r"null\s+'(.*)'", rest)
                if m:
                    session.null_display = m.group(1)
                elif re.match(r"expanded\b|x\b", rest):
                    arg = rest.split()[1] if len(rest.split()) > 1 else ""
                    session.expanded = psql_bool(arg) if arg else not session.expanded
            elif cmd == "\\x":
                session.expanded = psql_bool(rest) if rest else not session.expanded
            continue

        if not active():
            continue

        if kind == "copy_stdin":
            sql, _t, _c, data_lines = text
            blocks = expected_after_echo(sql)
            if blocks is None:
                continue
            actual, wedge = execute(
                lambda: session.conn.copy_stdin(interpolate(sql, session.pvars), data_lines), sql)
            if wedge:
                results.append((sql, rr.Verdict("REAL-FAIL", "server: " + wedge), wedge))
                continue
            exp_err = any(b.kind == "error" for b in blocks)
            if actual["err_codes"] and not exp_err:
                v = rr.Verdict("REAL-FAIL", "COPY error %s" % actual["err_codes"])
            elif exp_err and not actual["err_codes"]:
                v = rr.Verdict("REAL-FAIL", "expected error, COPY succeeded")
            else:
                v = rr.Verdict("PASS")
            results.append((sql, v, (actual.get("err_msgs") or [""])[0]))
            continue

        stmt = text
        exec_stmt = interpolate(stmt, session.pvars)
        blocks = expected_after_echo(stmt)
        if blocks is None:
            # Echo not found (psql-only syntax the splitter cannot model,
            # e.g. \bind ... \g or \gx). Still execute it -- later
            # statements may depend on its side effects -- but leave it
            # unscored, and count it so the report shows the blind spot.
            actual, wedge = execute(
                lambda: session.conn.q(rr.psql_unescape(exec_stmt)), stmt)
            results.append((stmt, rr.Verdict(
                "SKIP", "echo not found in .out (executed, unscored)"), ""))
            continue

        slow = rr.classify_too_slow(exec_stmt)
        if slow:
            results.append((stmt, rr.Verdict("EXPECTED-FAIL", slow), ""))
            continue

        gset = None
        m = re.search(r"\\gset(?:\s+([A-Za-z_]\w*))?\s*;?\s*$", exec_stmt)
        if m:
            gset = m.group(1) or ""
            exec_stmt = exec_stmt[: m.start()].rstrip()
            if not exec_stmt.endswith(";"):
                exec_stmt += ";"

        # Server-side COPY FROM a vendored data file: send as FROM STDIN.
        mcopy = COPY_FROM_FILE.match(exec_stmt)
        if mcopy:
            data = load_data_file(mcopy.group(3))
            if data is not None:
                sql = "COPY %s%s FROM STDIN%s" % (
                    mcopy.group(1), mcopy.group(2) or "", mcopy.group(4))
                actual, wedge = execute(lambda: session.conn.copy_stdin(sql, data), stmt)
                if wedge:
                    results.append((stmt, rr.Verdict("REAL-FAIL", "server: " + wedge), wedge))
                    continue
                exp_err = any(b.kind == "error" for b in blocks)
                if bool(actual["err_codes"]) == exp_err:
                    v = rr.Verdict("SKIP", "harness data load (COPY FROM file via STDIN)")
                else:
                    v = rr.Verdict("REAL-FAIL", "data load: COPY error %s" % actual["err_codes"]
                                   if actual["err_codes"] else "expected error, COPY succeeded")
                results.append((stmt, v, (actual.get("err_msgs") or [""])[0]))
                continue

        actual, wedge = execute(lambda: session.conn.q(rr.psql_unescape(exec_stmt)), stmt)
        if wedge:
            results.append((stmt, rr.Verdict("REAL-FAIL", "server: " + wedge), wedge))
            continue

        if gset is not None and not actual["err_codes"]:
            if actual["rows"]:
                for cname, cval in zip(actual["colnames"], actual["rows"][0]):
                    if cval is None:
                        session.pvars.pop(gset + cname, None)
                    else:
                        session.pvars[gset + cname] = cval
            actual["sets"] = []

        err_msg = (actual.get("err_msgs") or [""])[0]
        if session.expanded and not actual["err_codes"] and not any(
                b.kind == "error" for b in blocks):
            results.append((stmt, rr.Verdict("SKIP", "expanded (\\x) output not parsed"), ""))
            continue

        v = rr.compare_multi(stmt, blocks, actual["sets"], session.null_display)

        # Cascade bookkeeping across suites.
        cname = created_name(exec_stmt)
        expected_ok = not any(b.kind == "error" for b in blocks)
        if cname and expected_ok:
            if actual["err_codes"]:
                failed_objects[cname.lower()] = (
                    "c" if v.status == "EXPECTED-FAIL" and v.detail.startswith(C_FUNC_REASON)
                    else "real")
            else:
                failed_objects.pop(cname.lower(), None)
        if v.status == "REAL-FAIL" and actual["err_codes"]:
            casc = next((o for o in failed_objects
                         if re.search(r"\b%s\b" % re.escape(o), exec_stmt, re.IGNORECASE)
                         and o != (cname or "").lower()), None)
            if casc:
                if failed_objects[casc] == "c":
                    v = rr.Verdict("EXPECTED-FAIL", "%s (%s)" % (C_CASCADE_REASON, casc))
                else:
                    v = rr.Verdict("REAL-FAIL", "cascade: %s failed earlier; %s" % (casc, v.detail))
        results.append((stmt, v, err_msg))
        if verbose and v.status != "PASS":
            print("    [%s] %s -- %s %s" % (v.status, stmt[:70].replace("\n", " "),
                                           v.detail, err_msg[:60]))
    return results


def summarize(results):
    c = Counter()
    for _, v, _ in results:
        if v.status == "SKIP":
            c["SKIP"] += 1
            continue
        c[v.status] += 1
        if v.status == "REAL-FAIL" and v.detail.startswith("cascade:"):
            c["CASCADE"] += 1
    return c


def main():
    only = None
    report_md = None
    report_json = None
    verbose = False
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--tests" and i + 1 < len(args):
            only = set(args[i + 1].split(","))
            i += 2
        elif a == "--report" and i + 1 < len(args):
            report_md = args[i + 1]
            i += 2
        elif a == "--json" and i + 1 < len(args):
            report_json = args[i + 1]
            i += 2
        elif a == "--release":
            rr.BIN = os.path.join(ROOT, "target", "release", "rustgres")
            i += 1
        elif a == "--verbose":
            verbose = True
            i += 1
        else:
            print("unknown arg %s" % a)
            return 2
    if not os.path.exists(rr.BIN):
        print("missing %s; run cargo build first" % rr.BIN)
        return 2

    tests = schedule_tests()
    server = SharedServer()
    server.start()
    failed_objects = {}
    per_suite = {}
    recovery_failures = []
    t_all = time.time()
    try:
        for name in tests:
            if name in EXCLUDED:
                continue
            if only is not None and name != "test_setup" and name not in only:
                continue
            t0 = time.time()
            try:
                session = Session(server)
            except (rr.WireError, OSError):
                server.restart()
                session = Session(server)
            try:
                results = run_suite(session, name, failed_objects, verbose=verbose)
            except ServerUnrecoverable as e:
                # The data dir no longer opens (a crash-recovery bug). Keep
                # it for diagnosis, start a fresh cluster, rebuild the
                # baseline objects (unscored), and carry on: later suites
                # lose earlier suites' objects, which shows up as cascades.
                print("    ! %s: %s; continuing on a fresh data dir" % (name, e),
                      flush=True)
                recovery_failures.append({"suite": name, "error": str(e),
                                          "data_dir": server.data_dir,
                                          "log": server.log_path})
                server.stop()
                server = SharedServer()
                server.start()
                failed_objects.clear()
                setup = Session(server)
                run_suite(setup, "test_setup", failed_objects)
                setup.conn.close()
                results = [(name, rr.Verdict(
                    "REAL-FAIL", "server: unrecoverable after crash"), str(e))]
            try:
                session.conn.close()
            except Exception:
                pass
            per_suite[name] = results
            c = summarize(results)
            print("%-28s %6.1fs  PASS=%d EF=%d RF=%d (cascade %d) SKIP=%d" % (
                name, time.time() - t0, c["PASS"], c["EXPECTED-FAIL"],
                c["REAL-FAIL"], c["CASCADE"], c["SKIP"]), flush=True)
    finally:
        server.stop()

    totals = Counter()
    for res in per_suite.values():
        totals.update(summarize(res))
    scored = totals["PASS"] + totals["EXPECTED-FAIL"] + totals["REAL-FAIL"]
    rate = 100.0 * totals["PASS"] / scored if scored else 0.0
    print("\n==== PG19 SCHEDULE SUMMARY (%d suites, %.0fs) ====" % (
        len(per_suite), time.time() - t_all))
    print("scored %d  PASS=%d (%.1f%%)  EXPECTED-FAIL=%d  REAL-FAIL=%d "
          "(of which cascade %d)  unscored SKIP=%d" % (
              scored, totals["PASS"], rate, totals["EXPECTED-FAIL"],
              totals["REAL-FAIL"], totals["CASCADE"], totals["SKIP"]))

    for rf in recovery_failures:
        print("recovery failure during %s: %s\n  data dir kept: %s"
              % (rf["suite"], rf["error"], rf["data_dir"]))
    if report_json:
        data = {
            "upstream": open(os.path.join(PG19, "UPSTREAM")).read(),
            "recovery_failures": recovery_failures,
            "excluded": EXCLUDED,
            "totals": dict(totals),
            "suites": {
                name: {
                    "counts": dict(summarize(res)),
                    "failures": [
                        {"status": v.status, "detail": v.detail, "error": msg,
                         "stmt": stmt[:300]}
                        for stmt, v, msg in res if v.status not in ("PASS", "SKIP")
                    ],
                }
                for name, res in per_suite.items()
            },
        }
        with open(report_json, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=1)
    if report_md:
        with open(report_md, "w", encoding="utf-8") as f:
            f.write("# rustgres vs PG19 full regression schedule\n\n")
            f.write("Scored %d, PASS %d (%.1f%%), EXPECTED-FAIL %d, REAL-FAIL %d "
                    "(cascade %d), unscored %d\n\n" % (
                        scored, totals["PASS"], rate, totals["EXPECTED-FAIL"],
                        totals["REAL-FAIL"], totals["CASCADE"], totals["SKIP"]))
            for rf in recovery_failures:
                f.write("**Server unrecoverable during `%s`:** %s. The run "
                        "continued on a fresh data dir.\n\n" % (rf["suite"], rf["error"]))
            f.write("| suite | PASS | EXPECTED-FAIL | REAL-FAIL | cascade | pass % |\n")
            f.write("|---|---:|---:|---:|---:|---:|\n")
            for name, res in per_suite.items():
                c = summarize(res)
                s = c["PASS"] + c["EXPECTED-FAIL"] + c["REAL-FAIL"]
                f.write("| %s | %d | %d | %d | %d | %s |\n" % (
                    name, c["PASS"], c["EXPECTED-FAIL"], c["REAL-FAIL"], c["CASCADE"],
                    ("%.0f" % (100.0 * c["PASS"] / s)) if s else "-"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
