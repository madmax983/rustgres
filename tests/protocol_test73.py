#!/usr/bin/env python3
r"""v0.72 protocol tests: DML/DDL conformance sweep.

RED on the v0.71 base (828d231c01c537343ab1c3caebd5df0e789c7fee),
GREEN after v0.72.

One coherent theme: PostgreSQL 19 table-definition and row-routing
semantics that insert.sql-style conformance trips over —
1. Partition routing correctness:
   - RANGE partition key containing NULL is rejected (23514) unless a
     DEFAULT partition exists (PG routes NULL range keys to DEFAULT;
     a bare RANGE partition never accepts NULL).
   - HASH NULL still maps to remainder 0 (behavior pin).
   - A partitioned intermediate with no leaf children is not a leaf:
     direct INSERT is 23514 ("no partition of relation ... found").
   - `DROP TABLE parent, child` drops each object once (PG's
     performMultipleDeletions); the child's implicit drop with the
     parent is not an error.
2. `CREATE TABLE ... (LIKE src [INCLUDING|EXCLUDING ...])` (PG19
   semantics, grounded in gram.y/parse_utilcmd.c): bare LIKE copies
   column names/types and NOT NULL constraints only — defaults and
   CHECK constraints require INCLUDING DEFAULTS / INCLUDING
   CONSTRAINTS (or INCLUDING ALL). Options apply in order; later
   ones win.
3. `CREATE TABLE ... WITH (fillfactor = N)`: accepted; out-of-range
   fillfactor is 22023.
4. Comma-separated `ALTER TABLE ... ADD a int, ADD b int` in one
   statement.
5. Set-returning functions in VALUES (PG19 ExecProjectSRF semantics):
   `VALUES (generate_series(1,3))` expands to 3 rows;
   `INSERT ... VALUES (generate_series(1,3))` inserts 3 rows; an empty
   SRF yields zero rows without error. Multiple SRFs zip positionally
   with NULL padding to the longest (`VALUES (g(1,3), g(1,2))` ->
   (1,1),(2,2),(3,NULL)); an empty SRF alongside a non-empty one
   NULL-pads (`VALUES (g(5,4), g(1,2))` -> (NULL,1),(NULL,2)); scalar
   cells repeat on every row (`VALUES (42, g(1,2))` -> (42,1),(42,2)).

Requires a running rustgres server on port 5433.
Run: python3 tests/protocol_test73.py
"""
import socket
import struct
import sys

PORT = 5433


def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c:
            raise RuntimeError("connection closed")
        d += c
    return d


def read_msg(s):
    typ = read_exact(s, 1)
    ln = struct.unpack("!i", read_exact(s, 4))[0]
    payload = read_exact(s, ln - 4)
    return typ, payload


def startup(s):
    msg = struct.pack("!II", 196608, 0) + b"user\x00test\x00\x00"
    s.sendall(struct.pack("!I", len(msg) + 4) + msg)
    while True:
        typ, _ = read_msg(s)
        if typ == b"Z":
            break


def parse_error(payload):
    code, message, detail = None, None, None
    i = 0
    while i < len(payload) - 1:
        ftype = payload[i : i + 1]
        end = payload.find(b"\x00", i + 1)
        val = payload[i + 1 : end].decode()
        if ftype == b"C":
            code = val
        elif ftype == b"M":
            message = val
        elif ftype == b"D":
            detail = val
        i = end + 1
    return code, message, detail


def run(s, sql):
    """Run one simple-protocol query.

    Returns ("ok", tag, rows, None) or ("error", code, message, detail).
    rows is a list of row tuples (text values, None for NULL).
    """
    s.sendall(struct.pack("!cI", b"Q", len(sql) + 5) + sql.encode() + b"\x00")
    tag, rows = None, []
    while True:
        typ, payload = read_msg(s)
        if typ == b"C":
            tag = payload[:-1].decode()
        elif typ == b"D":
            nfields = struct.unpack("!H", payload[:2])[0]
            pos = 2
            row = []
            for _ in range(nfields):
                ln = struct.unpack("!i", payload[pos : pos + 4])[0]
                pos += 4
                row.append(payload[pos : pos + ln].decode() if ln >= 0 else None)
                pos += max(ln, 0)
            rows.append(tuple(row))
        elif typ == b"E":
            code, message, detail = parse_error(payload)
            # drain to ReadyForQuery
            while True:
                t2, _ = read_msg(s)
                if t2 == b"Z":
                    break
            return ("error", code, message, detail)
        elif typ == b"Z":
            break
    return ("ok", tag, rows, None)


CHECKS = 0


def check(cond, label, detail=""):
    global CHECKS
    CHECKS += 1
    if not cond:
        print(f"FAIL: {label} {detail}")
        raise SystemExit(1)
    print(f"PASS: {label}")


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    try:
        startup(s)

        # --- 1-2. RANGE NULL routing ---------------------------------
        # NOTE: the leaf bound is (1)->(MAXVALUE): on v0.71 a NULL key
        # slipped through `range_bound_contains` here (NULL compared
        # "incomparable" against the lower bound, then `key < +inf`
        # short-circuited true), so the NULL row was wrongly stored.
        # PG19 never matches a NULL range key to a non-default
        # partition: it goes to DEFAULT if one exists, else 23514.
        st, tag, rows, _d = run(
            s, "create table p73_r (a int, b int) partition by range (b)"
        )
        check(st == "ok", "create range parent", f"st={st}")
        st, tag, rows, _d = run(
            s,
            "create table p73_r1 partition of p73_r "
            "for values from (1) to (maxvalue)",
        )
        check(st == "ok", "create range leaf", f"st={st}")
        r = run(s, "insert into p73_r values (1, null)")
        check(
            r[0] == "error" and r[1] == "23514"
            and 'no partition of relation "p73_r" found for row' in r[2],
            "RANGE NULL with no DEFAULT -> 23514",
            f"r={r}",
        )
        st, tag, rows, _d = run(
            s, "create table p73_rd partition of p73_r default"
        )
        check(st == "ok", "create default partition", f"st={st}")
        st, tag, rows, _d = run(s, "insert into p73_r values (1, null)")
        check(st == "ok" and tag == "INSERT 0 1", "RANGE NULL -> DEFAULT ok",
              f"st={st} tag={tag}")
        st, tag, rows, _d = run(s, "select count(*) from p73_rd")
        check(
            st == "ok" and rows == [("1",)],
            "NULL row landed in the DEFAULT partition",
            f"rows={rows}",
        )

        # --- 3. childless partitioned intermediate --------------------
        st, tag, rows, _d = run(
            s, "create table p73_k (a int, b int) partition by range (b)"
        )
        check(st == "ok", "create multilevel root", f"st={st}")
        st, tag, rows, _d = run(
            s,
            "create table p73_k1 partition of p73_k "
            "for values from (1) to (10) partition by range (a)",
        )
        check(st == "ok", "create childless intermediate", f"st={st}")
        r = run(s, "insert into p73_k1 values (5, 5)")
        check(
            r[0] == "error" and r[1] == "23514"
            and 'no partition of relation "p73_k1" found for row' in r[2],
            "insert into childless partitioned intermediate -> 23514",
            f"r={r}",
        )
        st, tag, rows, _d = run(s, "select count(*) from p73_k1")
        check(
            st == "ok" and rows == [("0",)],
            "no row was stored in the intermediate",
            f"rows={rows}",
        )

        # --- 4. HASH NULL -> remainder 0 (behavior pin) ----------------
        stmts = [
            "create table p73_h (a int, b text) partition by hash (a)",
            "create table p73_h0 partition of p73_h for values with (modulus 4, remainder 0)",
            "create table p73_h1 partition of p73_h for values with (modulus 4, remainder 1)",
            "create table p73_h2 partition of p73_h for values with (modulus 4, remainder 2)",
            "create table p73_h3 partition of p73_h for values with (modulus 4, remainder 3)",
        ]
        for stmt in stmts:
            st, tag, rows, _d = run(s, stmt)
            check(st == "ok", f"hash setup: {stmt[:48]}", f"st={st}")
        st, tag, rows, _d = run(s, "insert into p73_h values (null, 'n')")
        check(st == "ok" and tag == "INSERT 0 1", "HASH NULL inserts ok",
              f"st={st} tag={tag}")
        st, tag, rows, _d = run(s, "select count(*) from p73_h0")
        check(
            st == "ok" and rows == [("1",)],
            "HASH NULL routed to remainder 0",
            f"rows={rows}",
        )

        # --- 4b. PARTITION OF + propagated ALTER (crash regression) -----
        # v0.72: CREATE TABLE ... PARTITION OF did not inherit the
        # parent's per-column COMPRESSION metadata, leaving
        # col_compression=[] against N columns. The next ALTER that
        # propagated to the new partition tripped a debug_assert and
        # killed the connection. The propagated ALTERs below must all
        # succeed without dropping the connection.
        stmts = [
            "create table p73_p (a int, b int) partition by range (a)",
            "create table p73_c (c text, a int not null, b int not null)"
            " partition by list (c)",
            "alter table p73_p attach partition p73_c"
            " for values from (1) to (10)",
            "alter table p73_p add d int, add e int",
            "alter table p73_p drop e",
            "create table p73_cab partition of p73_c"
            " for values in ('a', 'b') partition by range (c)",
            "create table p73_ca partition of p73_cab"
            " for values from ('a') to ('b')",
        ]
        for stmt in stmts:
            st, tag, rows, _d = run(s, stmt)
            check(st == "ok", f"alter-crash setup: {stmt[:52]}", f"st={st}")
        st, tag, rows, _d = run(s, "alter table p73_p drop d")
        check(
            st == "ok" and tag == "ALTER TABLE",
            "propagated DROP COLUMN over PARTITION OF tables",
            f"st={st} tag={tag}",
        )
        st, tag, rows, _d = run(
            s, "insert into p73_ca values ('a', 2, 7)"
        )
        check(st == "ok" and tag == "INSERT 0 1", "leaf still writable",
              f"st={st} tag={tag}")

        # --- 5. multi-name DROP TABLE ---------------------------------
        st, tag, rows, _d = run(
            s, "create table p73_dp (a int) partition by list (a)"
        )
        check(st == "ok", "create drop-test parent", f"st={st}")
        st, tag, rows, _d = run(
            s, "create table p73_dc partition of p73_dp for values in (1)"
        )
        check(st == "ok", "create drop-test child", f"st={st}")
        st, tag, rows, _d = run(s, "drop table p73_dp, p73_dc")
        check(st == "ok" and tag == "DROP TABLE", "DROP parent, child -> ok",
              f"st={st} tag={tag}")
        r = run(s, "select count(*) from p73_dc")
        check(
            r[0] == "error" and r[1] == "42P01",
            "child is really gone after multi-drop",
            f"r={r}",
        )

        # --- 6-7. CREATE TABLE ... LIKE (PG19 semantics) -----------------
        st, tag, rows, _d = run(
            s,
            "create table p73_src (a int not null, b text, "
            "c float default 1.5, constraint p73_src_a_check check (a > 0))",
        )
        check(st == "ok", "create LIKE source", f"st={st}")
        # Bare LIKE: columns + NOT NULL only. No default, no CHECK.
        st, tag, rows, _d = run(s, "create table p73_like (like p73_src)")
        check(st == "ok", "bare LIKE copies the definition", f"st={st}")
        st, tag, rows, _d = run(s, "insert into p73_like values (1, 'x')")
        check(st == "ok", "insert into LIKE table", f"st={st}")
        st, tag, rows, _d = run(
            s, "insert into p73_like (a, b) values (2, 'y') returning c"
        )
        check(
            st == "ok" and rows == [(None,)],
            "bare LIKE does not copy the default",
            f"rows={rows}",
        )
        r = run(s, "insert into p73_like values (-1, 'bad')")
        check(
            r[0] == "ok",
            "bare LIKE does not copy the CHECK",
            f"r={r}",
        )
        r = run(s, "insert into p73_like values (NULL, 'nonull')")
        check(
            r[0] == "error" and r[1] == "23502",
            "bare LIKE still copies NOT NULL",
            f"r={r}",
        )
        # INCLUDING DEFAULTS copies the default (but not the CHECK).
        st, tag, rows, _d = run(
            s,
            "create table p73_like2 "
            "(like p73_src including defaults excluding constraints)",
        )
        check(st == "ok", "LIKE with options", f"st={st}")
        st, tag, rows, _d = run(s, "insert into p73_like2 values (-5, 'neg')")
        check(
            st == "ok",
            "EXCLUDING CONSTRAINTS drops the CHECK",
            f"st={st}",
        )
        st, tag, rows, _d = run(
            s, "insert into p73_like2 (a, b) values (3, 'z') returning c"
        )
        check(
            st == "ok" and rows == [("1.5",)],
            "INCLUDING DEFAULTS keeps the default",
            f"rows={rows}",
        )
        # INCLUDING CONSTRAINTS copies the CHECK (but not the default).
        st, tag, rows, _d = run(
            s, "create table p73_like3 (like p73_src including constraints)"
        )
        check(st == "ok", "LIKE INCLUDING CONSTRAINTS", f"st={st}")
        r = run(s, "insert into p73_like3 values (-1, 'bad')")
        check(
            r[0] == "error" and r[1] == "23514",
            "INCLUDING CONSTRAINTS copies the CHECK",
            f"r={r}",
        )
        st, tag, rows, _d = run(
            s, "insert into p73_like3 (a, b) values (4, 'w') returning c"
        )
        check(
            st == "ok" and rows == [(None,)],
            "INCLUDING CONSTRAINTS does not copy the default",
            f"rows={rows}",
        )
        # INCLUDING ALL copies both; a later EXCLUDING wins.
        st, tag, rows, _d = run(
            s,
            "create table p73_like4 "
            "(like p73_src including all excluding defaults)",
        )
        check(st == "ok", "LIKE INCLUDING ALL EXCLUDING DEFAULTS", f"st={st}")
        r = run(s, "insert into p73_like4 values (-2, 'bad')")
        check(
            r[0] == "error" and r[1] == "23514",
            "INCLUDING ALL keeps the CHECK",
            f"r={r}",
        )
        st, tag, rows, _d = run(
            s, "insert into p73_like4 (a, b) values (5, 'v') returning c"
        )
        check(
            st == "ok" and rows == [(None,)],
            "EXCLUDING DEFAULTS drops the default",
            f"rows={rows}",
        )
        # PG19 MergeAttributes: LIKE/explicit column collision is 42701.
        r = run(s, "create table p73_like5 (a int, like p73_src)")
        check(
            r[0] == "error" and r[1] == "42701"
            and 'column "a" specified more than once' in r[2],
            "LIKE column colliding with explicit column -> 42701",
            f"r={r}",
        )
        r = run(s, "create table p73_like6 (like p73_src, like p73_src)")
        check(
            r[0] == "error" and r[1] == "42701",
            "two LIKE clauses colliding -> 42701",
            f"r={r}",
        )

        # --- 8-9. reloptions -------------------------------------------
        st, tag, rows, _d = run(
            s, "create table p73_ff (a int) with (fillfactor = 10)"
        )
        check(st == "ok", "WITH (fillfactor=10) accepted", f"st={st}")
        r = run(s, "create table p73_ff2 (a int) with (fillfactor = 101)")
        check(
            r[0] == "error" and r[1] == "22023",
            "fillfactor 101 -> 22023",
            f"r={r}",
        )
        r = run(s, "create table p73_ff3 (a int) with (fillfactor = 9)")
        check(
            r[0] == "error" and r[1] == "22023",
            "fillfactor 9 -> 22023",
            f"r={r}",
        )

        # --- 10. multi-action ALTER -------------------------------------
        st, tag, rows, _d = run(s, "create table p73_alt (a int)")
        check(st == "ok", "create ALTER target", f"st={st}")
        st, tag, rows, _d = run(
            s, "alter table p73_alt add b int, add c text"
        )
        check(
            st == "ok" and tag == "ALTER TABLE",
            "comma-separated ALTER actions",
            f"st={st} tag={tag}",
        )
        st, tag, rows, _d = run(
            s, "insert into p73_alt values (1, 2, 'three')"
        )
        check(st == "ok" and tag == "INSERT 0 1", "insert into widened table",
              f"st={st} tag={tag}")

        # --- 11-13. SRFs in VALUES --------------------------------------
        st, tag, rows, _d = run(s, "values (generate_series(1, 3))")
        check(
            st == "ok" and rows == [("1",), ("2",), ("3",)],
            "VALUES (generate_series(1,3)) expands",
            f"rows={rows}",
        )
        st, tag, rows, _d = run(s, "create table p73_gs (x int)")
        check(st == "ok", "create SRF target", f"st={st}")
        st, tag, rows, _d = run(
            s, "insert into p73_gs values (generate_series(1, 3))"
        )
        check(
            st == "ok" and tag == "INSERT 0 3",
            "INSERT ... VALUES (SRF) inserts 3 rows",
            f"st={st} tag={tag}",
        )
        st, tag, rows, _d = run(
            s, "insert into p73_gs values (generate_series(5, 4))"
        )
        check(
            st == "ok" and tag == "INSERT 0 0",
            "empty SRF inserts zero rows without error",
            f"st={st} tag={tag}",
        )
        st, tag, rows, _d = run(s, "select count(*) from p73_gs")
        check(
            st == "ok" and rows == [("3",)],
            "SRF target holds exactly the 3 expanded rows",
            f"rows={rows}",
        )
        # --- 14-17. SRF zip / NULL-pad / scalar-repeat (ProjectSet) -----
        st, tag, rows, _d = run(
            s, "values (42, generate_series(1, 2))"
        )
        check(
            st == "ok" and rows == [("42", "1"), ("42", "2")],
            "scalar cell repeats on every SRF row",
            f"rows={rows}",
        )
        st, tag, rows, _d = run(
            s, "values (generate_series(1, 3), generate_series(1, 2))"
        )
        check(
            st == "ok"
            and rows == [("1", "1"), ("2", "2"), ("3", None)],
            "multiple SRFs zip with NULL padding to the longest",
            f"rows={rows}",
        )
        st, tag, rows, _d = run(
            s, "values (generate_series(5, 4), generate_series(1, 2))"
        )
        check(
            st == "ok" and rows == [(None, "1"), (None, "2")],
            "empty SRF alongside a non-empty one NULL-pads",
            f"rows={rows}",
        )
        st, tag, rows, _d = run(s, "create table p73_gs2 (a int, x int)")
        check(st == "ok", "create SRF zip target", f"st={st}")
        st, tag, rows, _d = run(
            s, "insert into p73_gs2 values (7, generate_series(1, 3))"
        )
        check(
            st == "ok" and tag == "INSERT 0 3",
            "INSERT VALUES (scalar, SRF) inserts 3 rows",
            f"st={st} tag={tag}",
        )
        st, tag, rows, _d = run(
            s, "select a, x from p73_gs2 order by x"
        )
        check(
            st == "ok" and rows == [("7", "1"), ("7", "2"), ("7", "3")],
            "scalar repeats in INSERT SRF expansion",
            f"rows={rows}",
        )

        print(f"\nALL {CHECKS} CHECKS PASSED")
    finally:
        s.close()


if __name__ == "__main__":
    sys.exit(main())
