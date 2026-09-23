#!/usr/bin/env python3
r"""v0.69 protocol tests: declarative partitioning (PG19 core).

RED on the v0.68 base (fd82ee1e), GREEN after v0.69.

Covers the v0.69 scope:
1. CREATE TABLE ... PARTITION BY LIST with PARTITION OF children.
2. INSERT into the parent routes to the correct child (list).
3. SELECT from the parent sees rows in all children.
4. Direct INSERT into a child violating its bound -> 23514.
5. INSERT with no matching partition -> 23514.
6. RANGE partitioning: routing + bound enforcement + MINVALUE/MAXVALUE
   bound rules (invalid bounds rejected at CREATE).
7. ALTER TABLE ... ATTACH PARTITION.
8. DROP TABLE on the parent removes children recursively.

Requires a running rustgres server on port 5433.
Run: python3 tests/protocol_test70.py
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

        # --- 1. LIST partitioning: DDL --------------------------------
        st, tag, rows = run(s, "create table p70_list (a int, b text) partition by list (a)")
        check(st == "ok", "create list-partitioned parent", f"st={st} tag={tag}")
        st, tag, rows = run(s, "create table p70_list1 partition of p70_list for values in (1, 2)")
        check(st == "ok", "create list child 1", f"st={st}")
        st, tag, rows = run(s, "create table p70_list2 partition of p70_list for values in (3)")
        check(st == "ok", "create list child 2", f"st={st}")

        # --- 2. INSERT routing ----------------------------------------
        st, tag, rows = run(s, "insert into p70_list values (1, 'one'), (3, 'three'), (2, 'two')")
        check(st == "ok" and tag == "INSERT 0 3", "routed multi-row insert", f"st={st} tag={tag}")

        # --- 3. parent scan -------------------------------------------
        st, tag, rows = run(s, "select a from p70_list order by a")
        check(st == "ok" and rows == ["1", "2", "3"], "parent scan sees all children", f"rows={rows}")

        # --- 4. direct child bound violation -> 23514 -----------------
        st, code, msg = run(s, "insert into p70_list1 values (9, 'nine')")
        check(st == "error" and code == "23514", "child bound violation -> 23514",
              f"st={st} code={code} msg={msg}")

        # --- 5. no matching partition -> 23514 ------------------------
        st, code, msg = run(s, "insert into p70_list values (9, 'nine')")
        check(st == "error" and code == "23514", "no partition for row -> 23514",
              f"st={st} code={code} msg={msg}")

        # --- 6. RANGE partitioning -------------------------------------
        st, tag, rows = run(s, "create table p70_range (a int, b int) partition by range (a)")
        check(st == "ok", "create range-partitioned parent", f"st={st}")
        st, tag, rows = run(s, "create table p70_r1 partition of p70_range for values from (0) to (100)")
        check(st == "ok", "create range child [0,100)", f"st={st}")
        st, tag, rows = run(s, "create table p70_r2 partition of p70_range for values from (100) to (200)")
        check(st == "ok", "create range child [100,200)", f"st={st}")
        st, tag, rows = run(s, "insert into p70_range values (1, 1), (150, 2)")
        check(st == "ok", "routed range insert", f"st={st} tag={tag}")
        st, tag, rows = run(s, "select a from p70_range order by a")
        check(st == "ok" and rows == ["1", "150"], "range parent scan", f"rows={rows}")
        # upper bound is exclusive
        st, code, msg = run(s, "insert into p70_r1 values (100, 1)")
        check(st == "error" and code == "23514", "range upper exclusive -> 23514",
              f"st={st} code={code}")

        # MINVALUE/MAXVALUE bound rule: bound after MINVALUE must be MINVALUE.
        st, code, msg = run(
            s, "create table p70_bad partition of p70_range for values from (minvalue, 0) to (1, maxvalue)"
        )
        check(st == "error" and code == "42601", "bad minvalue bound rejected",
              f"st={st} code={code} msg={msg}")
        # the failed CREATE must not leave a table behind
        st, code, msg = run(s, "insert into p70_bad values (0, 0)")
        check(st == "error" and code == "42P01", "failed partition create left nothing",
              f"st={st} code={code}")

        # --- 7. ATTACH PARTITION --------------------------------------
        st, tag, rows = run(s, "create table p70_det (a int, b text)")
        check(st == "ok", "create detached table", f"st={st}")
        st, tag, rows = run(s, "insert into p70_det values (4, 'det')")
        check(st == "ok", "insert into detached table", f"st={st}")
        st, tag, rows = run(
            s, "alter table p70_list attach partition p70_det for values in (4)"
        )
        check(st == "ok", "attach partition", f"st={st} msg={rows}")
        st, tag, rows = run(s, "insert into p70_list values (4, 'four')")
        check(st == "ok", "insert routes to attached partition", f"st={st}")
        st, tag, rows = run(s, "select a from p70_list order by a")
        check(st == "ok" and rows == ["1", "2", "3", "4", "4"], "scan after attach", f"rows={rows}")

        # --- 8. recursive DROP ----------------------------------------
        st, tag, rows = run(s, "drop table p70_list")
        check(st == "ok", "drop partitioned parent", f"st={st}")
        st, code, msg = run(s, "select * from p70_list1")
        check(st == "error" and code == "42P01", "child gone after parent drop",
              f"st={st} code={code}")
        st, code, msg = run(s, "select * from p70_det")
        check(st == "error" and code == "42P01", "attached child gone after parent drop",
              f"st={st} code={code}")

        # --- 9. HASH partitioning -------------------------------------
        st, tag, rows = run(s, "create table p70_hash (a int) partition by hash (a)")
        check(st == "ok", "create hash-partitioned parent", f"st={st}")
        for r in range(4):
            st, tag, rows = run(
                s, f"create table p70_h{r} partition of p70_hash for values with (modulus 4, remainder {r})"
            )
            check(st == "ok", f"create hash child remainder {r}", f"st={st}")
        st, tag, rows = run(s, "insert into p70_hash values (0),(1),(2),(3),(4),(5),(6),(7)")
        check(st == "ok" and tag == "INSERT 0 8", "hash-routed insert", f"st={st} tag={tag}")
        st, tag, rows = run(s, "select a from p70_hash order by a")
        check(
            st == "ok" and rows == ["0", "1", "2", "3", "4", "5", "6", "7"],
            "hash parent scan",
            f"rows={rows}",
        )
        st, tag, rows = run(s, "drop table p70_hash")
        check(st == "ok", "drop hash parent", f"st={st}")

        # cleanup
        st, tag, rows = run(s, "drop table p70_range")
        check(st == "ok", "drop range parent", f"st={st}")

        print(f"\nprotocol_test70: {CHECKS} checks passed")
    finally:
        s.close()


if __name__ == "__main__":
    main()
