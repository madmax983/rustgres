#!/usr/bin/env python3
r"""v0.70 protocol tests: partitioning correctness (PG19 semantics).

RED on the v0.69 base (2877a5eeb4036cfade9ea1c00614a3c98a3d6097),
GREEN after v0.70.

Covers the v0.70 scope:
1. Expression partition keys without extra parentheses:
   PARTITION BY LIST (lower(a)) parses (v0.69: 42601).
2. DEFAULT partition: direct insert of a DEFAULT-only value succeeds;
   a value matching an explicit sibling is rejected (v0.69: every
   direct DEFAULT insert was rejected).
3. Multilevel partitioning: PARTITION OF ... PARTITION BY on an
   intermediate; routing uses each level's own key (v0.69: the root
   key was reused at every level).
4. Direct insert into a subpartitioned intermediate validates the
   ancestor bound and routes to descendant leaves.
5. ATTACH PARTITION is transactional: ROLLBACK unlinks the child
   (v0.69: the link leaked and the insert succeeded).
6. CREATE TABLE ... PARTITION OF is transactional: ROLLBACK removes it.
7. DROP TABLE of a partition is transactional: ROLLBACK relinks it.
8. UPDATE/DELETE naming a partitioned parent operate on all leaves
   (v0.69: they touched only the parent's own — zero — rows).
9. UPDATE changing the partition key moves the row between leaves.
10. ON CONFLICT on a partitioned table is cleanly rejected (0A000).
    [v0.71: now supported per PG19; assertion updated to expect success.]

Requires a running rustgres server on port 5433.
Run: python3 tests/protocol_test71.py
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
    code, message = None, None
    i = 0
    while i < len(payload) - 1:
        ftype = payload[i : i + 1]
        end = payload.find(b"\x00", i + 1)
        val = payload[i + 1 : end].decode()
        if ftype == b"C":
            code = val
        elif ftype == b"M":
            message = val
        i = end + 1
    return code, message


def run(s, sql):
    """Run one simple-protocol query.

    Returns ("ok", tag, rows) or ("error", code, message). rows is a
    list of first-column text values (None for NULL).
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
            ln = struct.unpack("!i", payload[pos : pos + 4])[0]
            pos += 4
            rows.append(payload[pos : pos + ln].decode() if ln >= 0 else None)
        elif typ == b"E":
            code, message = parse_error(payload)
            # drain to ReadyForQuery
            while True:
                t2, _ = read_msg(s)
                if t2 == b"Z":
                    break
            return ("error", code, message)
        elif typ == b"Z":
            break
    return ("ok", tag, rows)


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

        # --- 1. expression partition keys -----------------------------
        st, tag, rows = run(
            s, "create table p71_expr (a text, b int) partition by list (lower(a))"
        )
        check(st == "ok", "unparenthesized expression key parses", f"st={st} tag={tag}")
        st, tag, rows = run(
            s, "create table p71_expr_x partition of p71_expr for values in ('x')"
        )
        check(st == "ok", "create child of expression-partitioned parent", f"st={st}")
        st, tag, rows = run(s, "insert into p71_expr values ('X', 1)")
        check(st == "ok" and tag == "INSERT 0 1", "route via expression key",
              f"st={st} tag={tag}")
        st, tag, rows = run(s, "select b from p71_expr_x")
        check(st == "ok" and rows == ["1"], "row landed in expression-keyed leaf",
              f"rows={rows}")

        # --- 2. DEFAULT partition -------------------------------------
        st, tag, rows = run(s, "create table p71_def (a int) partition by list (a)")
        check(st == "ok", "create list parent for default test", f"st={st}")
        st, tag, rows = run(s, "create table p71_def1 partition of p71_def for values in (1)")
        check(st == "ok", "create explicit child", f"st={st}")
        st, tag, rows = run(s, "create table p71_defd partition of p71_def default")
        check(st == "ok", "create default child", f"st={st}")
        # a value only the DEFAULT accepts: direct insert must succeed
        st, tag, rows = run(s, "insert into p71_defd values (99)")
        check(st == "ok", "direct DEFAULT insert of default-only value", f"st={st}")
        # a value an explicit sibling accepts: direct DEFAULT insert fails
        st, code, msg = run(s, "insert into p71_defd values (1)")
        check(st == "error" and code == "23514",
              "direct DEFAULT insert of sibling value -> 23514",
              f"st={st} code={code} msg={msg}")
        # routing: parent insert of an unmatched value lands in DEFAULT
        st, tag, rows = run(s, "insert into p71_def values (42)")
        check(st == "ok", "parent insert routes unmatched value to default", f"st={st}")
        st, tag, rows = run(s, "select a from p71_defd order by a")
        check(st == "ok" and rows == ["42", "99"], "default leaf holds routed rows",
              f"rows={rows}")

        # --- 3. multilevel routing ------------------------------------
        st, tag, rows = run(
            s, "create table p71_ml (a text, b int) partition by list (a)"
        )
        check(st == "ok", "create multilevel root", f"st={st}")
        st, tag, rows = run(
            s, "create table p71_ml_ee partition of p71_ml for values in ('ee') partition by range (b)"
        )
        check(st == "ok", "create subpartitioned intermediate", f"st={st}")
        st, tag, rows = run(
            s, "create table p71_ml_ee1 partition of p71_ml_ee for values from (0) to (10)"
        )
        check(st == "ok", "create range leaf under intermediate", f"st={st}")
        st, tag, rows = run(
            s, "create table p71_ml_ee2 partition of p71_ml_ee for values from (10) to (20)"
        )
        check(st == "ok", "create second range leaf", f"st={st}")
        # root insert must use the root key (a) then the intermediate key (b)
        st, tag, rows = run(s, "insert into p71_ml values ('ee', 5)")
        check(st == "ok" and tag == "INSERT 0 1", "multilevel root insert routes",
              f"st={st} tag={tag} rows={rows}")
        st, tag, rows = run(s, "select b from p71_ml_ee1")
        check(st == "ok" and rows == ["5"], "row reached the range leaf", f"rows={rows}")
        st, tag, rows = run(s, "insert into p71_ml values ('ee', 15)")
        check(st == "ok", "second multilevel insert", f"st={st}")
        st, tag, rows = run(s, "select b from p71_ml_ee2")
        check(st == "ok" and rows == ["15"], "row reached the second leaf", f"rows={rows}")

        # --- 4. direct insert into the intermediate -------------------
        # validates the ancestor (list) bound, then routes by range
        st, tag, rows = run(s, "insert into p71_ml_ee values ('ee', 7)")
        check(st == "ok", "direct intermediate insert within bound", f"st={st}")
        st, code, msg = run(s, "insert into p71_ml_ee values ('xx', 7)")
        check(st == "error" and code == "23514",
              "direct intermediate insert violating ancestor bound -> 23514",
              f"st={st} code={code} msg={msg}")
        st, tag, rows = run(s, "select b from p71_ml_ee1 order by b")
        check(st == "ok" and rows == ["5", "7"], "intermediate insert routed to leaf",
              f"rows={rows}")

        # --- 5. ATTACH rollback ---------------------------------------
        st, tag, rows = run(s, "create table p71_rb (a int) partition by list (a)")
        check(st == "ok", "create rollback test parent", f"st={st}")
        st, tag, rows = run(s, "create table p71_rb_c (a int)")
        check(st == "ok", "create rollback test child", f"st={st}")
        st, tag, rows = run(s, "begin")
        check(st == "ok", "begin", f"st={st}")
        st, tag, rows = run(
            s, "alter table p71_rb attach partition p71_rb_c for values in (1)"
        )
        check(st == "ok", "attach inside txn", f"st={st}")
        st, tag, rows = run(s, "rollback")
        check(st == "ok", "rollback", f"st={st}")
        st, code, msg = run(s, "insert into p71_rb values (1)")
        check(st == "error" and code == "23514",
              "attach rolled back: no partition for row",
              f"st={st} code={code} msg={msg}")
        st, tag, rows = run(s, "insert into p71_rb_c values (1)")
        check(st == "ok", "detached child still a plain table", f"st={st} tag={tag}")

        # --- 6. CREATE PARTITION OF rollback --------------------------
        st, tag, rows = run(s, "begin")
        check(st == "ok", "begin", f"st={st}")
        st, tag, rows = run(
            s, "create table p71_rb_p partition of p71_rb for values in (2)"
        )
        check(st == "ok", "create partition of inside txn", f"st={st}")
        st, tag, rows = run(s, "rollback")
        check(st == "ok", "rollback", f"st={st}")
        st, code, msg = run(s, "insert into p71_rb_p values (2)")
        check(st == "error" and code == "42P01",
              "rolled-back partition of left no table",
              f"st={st} code={code}")

        # --- 7. DROP partition rollback -------------------------------
        st, tag, rows = run(
            s, "create table p71_rb_d partition of p71_rb for values in (3)"
        )
        check(st == "ok", "create partition to drop", f"st={st}")
        st, tag, rows = run(s, "insert into p71_rb values (3)")
        check(st == "ok", "insert into partition to drop", f"st={st}")
        st, tag, rows = run(s, "begin")
        check(st == "ok", "begin", f"st={st}")
        st, tag, rows = run(s, "drop table p71_rb_d")
        check(st == "ok", "drop partition inside txn", f"st={st}")
        st, tag, rows = run(s, "rollback")
        check(st == "ok", "rollback", f"st={st}")
        st, tag, rows = run(s, "select a from p71_rb_d")
        check(st == "ok" and rows == ["3"], "rolled-back drop relinked the child",
              f"st={st} rows={rows}")
        st, tag, rows = run(s, "insert into p71_rb values (3)")
        check(st == "ok", "routing works after drop rollback", f"st={st}")

        # --- 8. parent UPDATE/DELETE ----------------------------------
        st, tag, rows = run(s, "create table p71_ud (a int, b text) partition by list (a)")
        check(st == "ok", "create update/delete test parent", f"st={st}")
        st, tag, rows = run(
            s, "create table p71_ud1 partition of p71_ud for values in (1)"
        )
        check(st == "ok", "create ud child 1", f"st={st}")
        st, tag, rows = run(
            s, "create table p71_ud2 partition of p71_ud for values in (2)"
        )
        check(st == "ok", "create ud child 2", f"st={st}")
        st, tag, rows = run(
            s, "insert into p71_ud values (1, 'one'), (2, 'two')"
        )
        check(st == "ok", "seed ud rows", f"st={st}")
        st, tag, rows = run(s, "update p71_ud set b = 'uno' where a = 1")
        check(st == "ok" and tag == "UPDATE 1", "parent update hits leaf row",
              f"st={st} tag={tag}")
        st, tag, rows = run(s, "select b from p71_ud1")
        check(st == "ok" and rows == ["uno"], "leaf row updated", f"rows={rows}")
        st, tag, rows = run(s, "delete from p71_ud where a = 2")
        check(st == "ok" and tag == "DELETE 1", "parent delete hits leaf row",
              f"st={st} tag={tag}")
        st, tag, rows = run(s, "select a from p71_ud")
        check(st == "ok" and rows == ["1"], "leaf row deleted via parent",
              f"rows={rows}")

        # --- 9. partition-key update moves the row --------------------
        st, tag, rows = run(s, "update p71_ud set a = 2 where a = 1")
        check(st == "ok" and tag == "UPDATE 1", "key-changing update", f"st={st} tag={tag}")
        st, tag, rows = run(s, "select b from p71_ud1")
        check(st == "ok" and rows == [], "row left the old leaf", f"rows={rows}")
        st, tag, rows = run(s, "select a, b from p71_ud2")
        check(st == "ok" and rows == ["2"], "row arrived in the new leaf",
              f"rows={rows}")

        # --- 10. ON CONFLICT on partitioned (v0.71: supported) -----------
        # v0.70 rejected this with 0A000; v0.71 implements PG19
        # partitioned-parent ON CONFLICT, so DO NOTHING now succeeds.
        # (No unique constraint here: degrades to a plain insert.)
        st, tag, rows = run(
            s, "insert into p71_ud values (2, 'x') on conflict do nothing"
        )
        check(st == "ok" and tag == "INSERT 0 1",
              "on conflict do nothing on partitioned table succeeds (v0.71)",
              f"st={st} tag={tag}")

        # cleanup
        for t in ("p71_expr", "p71_def", "p71_ml", "p71_rb", "p71_ud"):
            st, tag, rows = run(s, f"drop table {t}")
            check(st == "ok", f"drop {t}", f"st={st}")

        print(f"\nprotocol_test71: {CHECKS} checks passed")
    finally:
        s.close()


if __name__ == "__main__":
    main()
