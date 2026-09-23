#!/usr/bin/env python3
r"""v0.71 protocol tests: PG19 partitioned-parent ON CONFLICT.

RED on the v0.70 base (b0fc11264fd063d6106e7e6c2581c7dc579d9de4),
GREEN after v0.71.

Covers the v0.71 scope (PostgreSQL 19 semantics: tuple routing
precedes ON CONFLICT arbitration; parent arbiter indexes map to each
leaf's backing index):
1. Plain duplicate INSERT into a partitioned parent is rejected
   (23505 naming the constraint) — v0.70 allowed the duplicate.
2. Parent ON CONFLICT (inferred key) DO NOTHING skips the conflict.
3. Parent ON CONFLICT DO NOTHING without an arbiter skips duplicates
   across leaves and inserts the rest.
4. Parent ON CONFLICT DO UPDATE updates the leaf row; RETURNING works.
5. ON CONFLICT ON CONSTRAINT names the parent's constraint.
6. An arbiter matching no parent constraint is 42P10.
7. DO UPDATE that would move the row to another partition is 0A000
   with the PG19 message and DETAIL.
8. DO UPDATE changing the key within the same leaf is allowed.
9. DO UPDATE ... WHERE false skips (INSERT 0 0).
10. One statement touching several leaves: per-row routing +
    arbitration, RETURNING in statement order.
11. Duplicate proposed rows in one statement: DO NOTHING keeps the first.
12. Column-order remapping: ATTACHed partition with shuffled columns;
    conflict search, DO UPDATE, and RETURNING all use parent order.
13. Multilevel partitioning: ON CONFLICT through a subpartitioned
    intermediate; second-level cross-partition move is 0A000.
14. Transactional: ROLLBACK undoes a partitioned upsert.

Requires a running rustgres server on port 5433.
Run: python3 tests/protocol_test72.py
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

        # --- setup -------------------------------------------------
        st, tag, rows, _detail = run(
            s,
            "create table p72_pt (a int primary key, b text) partition by list (a)",
        )
        check(st == "ok", "create partitioned parent", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_p1 partition of p72_pt for values in (1, 2)")
        check(st == "ok", "create leaf p72_p1", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_p2 partition of p72_pt for values in (3, 4)")
        check(st == "ok", "create leaf p72_p2", f"st={st}")
        st, tag, rows, _detail = run(s, "insert into p72_pt values (1, 'one'), (3, 'three')")
        check(st == "ok" and tag == "INSERT 0 2", "seed two leaves", f"tag={tag}")

        # --- 1. plain duplicate -> 23505 ---------------------------
        r = run(s, "insert into p72_pt values (1, 'dup')")
        check(
            r[0] == "error" and r[1] == "23505" and "p72_pt_pkey" in r[2],
            "plain duplicate into partitioned parent -> 23505",
            f"r={r}",
        )

        # --- 2. DO NOTHING, inferred arbiter -----------------------
        st, tag, rows, _detail = run(
            s, "insert into p72_pt values (1, 'uno') on conflict (a) do nothing"
        )
        check(st == "ok" and tag == "INSERT 0 0", "inferred arbiter do nothing skips",
              f"st={st} tag={tag}")
        st, tag, rows, _detail = run(s, "select b from p72_pt")
        check(st == "ok" and sorted(r[0] for r in rows) == ["one", "three"],
              "no duplicate inserted", f"rows={rows}")

        # --- 3. DO NOTHING, no arbiter, several leaves -------------
        st, tag, rows, _detail = run(
            s,
            "insert into p72_pt values (3, 'tres'), (4, 'four') on conflict do nothing",
        )
        check(st == "ok" and tag == "INSERT 0 1", "no-arbiter do nothing: 1 of 2",
              f"st={st} tag={tag}")
        st, tag, rows, _detail = run(s, "select b from p72_pt")
        check(st == "ok" and sorted(r[0] for r in rows) == ["four", "one", "three"],
              "dup skipped, new row inserted", f"rows={rows}")

        # --- 4. DO UPDATE ------------------------------------------
        st, tag, rows, _detail = run(
            s,
            "insert into p72_pt values (1, 'uno') on conflict (a) "
            "do update set b = excluded.b returning b",
        )
        check(st == "ok" and tag == "INSERT 0 1" and rows == [("uno",)],
              "do update modifies the leaf row", f"st={st} tag={tag} rows={rows}")
        st, tag, rows, _detail = run(s, "select b from p72_p1")
        check(st == "ok" and rows == [("uno",)], "leaf holds the update",
              f"rows={rows}")

        # --- 5. ON CONSTRAINT --------------------------------------
        st, tag, rows, _detail = run(
            s,
            "insert into p72_pt values (3, 'trois') on conflict on constraint p72_pt_pkey "
            "do update set b = excluded.b returning a, b",
        )
        check(
            st == "ok" and tag == "INSERT 0 1" and rows == [("3", "trois")],
            "on constraint resolves the parent constraint",
            f"st={st} tag={tag} rows={rows}",
        )

        # --- 6. unmapped arbiter -> 42P10 ---------------------------
        r = run(s, "insert into p72_pt values (1, 'x') on conflict (b) do nothing")
        check(r[0] == "error" and r[1] == "42P10",
              "arbiter on non-unique column -> 42P10", f"r={r}")
        r = run(
            s,
            "insert into p72_pt values (1, 'x') on conflict on constraint nope do nothing",
        )
        check(r[0] == "error" and r[1] == "42P10",
              "unknown constraint name -> 42P10", f"r={r}")

        # --- 7. cross-partition DO UPDATE -> 0A000 -------------------
        r = run(
            s,
            "insert into p72_pt values (1, 'moved') on conflict (a) do update set a = 3",
        )
        check(
            r[0] == "error"
            and r[1] == "0A000"
            and r[2] == "invalid ON UPDATE specification"
            and r[3]
            == "The result tuple would appear in a different partition than the original tuple.",
            "cross-partition do update -> 0A000 with PG19 detail",
            f"r={r}",
        )
        st, tag, rows, _detail = run(s, "select a, b from p72_pt")
        check(
            st == "ok" and sorted(rows) == [("1", "uno"), ("3", "trois"), ("4", "four")],
            "failed do update left the table unchanged",
            f"rows={rows}",
        )

        # --- 8. same-leaf key change is allowed ---------------------
        # (1,'uno') lives in p72_p1 (values in (1,2)); moving a=1->2
        # stays in the same leaf.
        st, tag, rows, _detail = run(
            s,
            "insert into p72_pt values (1, 'x') on conflict (a) "
            "do update set a = 2 returning a, b",
        )
        check(st == "ok" and tag == "INSERT 0 1" and rows == [("2", "uno")],
              "same-leaf partition-key update succeeds",
              f"st={st} tag={tag} rows={rows}")
        st, tag, rows, _detail = run(s, "select a from p72_p1")
        check(st == "ok" and sorted(r[0] for r in rows) == ["2",],
              "moved row lives in the same leaf", f"rows={rows}")

        # --- 9. DO UPDATE ... WHERE false skips ---------------------
        st, tag, rows, _detail = run(
            s,
            "insert into p72_pt values (2, 'zzz') on conflict (a) "
            "do update set b = excluded.b where false",
        )
        check(st == "ok" and tag == "INSERT 0 0", "do update where false skips",
              f"st={st} tag={tag}")
        st, tag, rows, _detail = run(s, "select b from p72_p1")
        check(st == "ok" and rows == [("uno",)],
              "skipped update changed nothing", f"rows={rows}")

        # --- 10. multi-row, multi-leaf, statement order -------------
        st, tag, rows, _detail = run(
            s,
            "insert into p72_pt values (2, 'two'), (3, 'tres'), (1, 'uno'), (4, 'quatre') "
            "on conflict (a) do update set b = excluded.b returning a",
        )
        check(
            st == "ok" and tag == "INSERT 0 4"
            and rows == [("2",), ("3",), ("1",), ("4",)],
            "four rows across two leaves, returning in statement order",
            f"st={st} tag={tag} rows={rows}",
        )

        # --- 11. duplicate proposed rows ----------------------------
        # Fresh table: two identical proposed rows, neither exists yet.
        # The first is planned as an insert; the second sees the
        # planned row and skips.
        st, tag, rows, _detail = run(
            s,
            "create table p72_dup (a int primary key, b text) partition by list (a)",
        )
        check(st == "ok", "create dup-test parent", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_dup1 partition of p72_dup for values in (1, 2)")
        check(st == "ok", "create dup-test leaf", f"st={st}")
        st, tag, rows, _detail = run(
            s,
            "insert into p72_dup values (1, 'a'), (1, 'b') on conflict (a) "
            "do nothing returning a, b",
        )
        check(st == "ok" and tag == "INSERT 0 1" and rows == [("1", "a")],
              "duplicate proposed rows: first wins", f"st={st} tag={tag} rows={rows}")
        st, tag, rows, _detail = run(s, "select b from p72_dup")
        check(st == "ok" and rows == [("a",)], "only one row stored",
              f"rows={rows}")
        st, tag, rows, _detail = run(s, "drop table p72_dup")
        check(st == "ok", "drop dup-test table", f"st={st}")

        # --- 12. column-order remapping via ATTACH ------------------
        st, tag, rows, _detail = run(
            s,
            "create table p72_m (a int primary key, b text, c int) partition by list (a)",
        )
        check(st == "ok", "create remap parent", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_m1 (a int primary key, b text, c int)")
        check(st == "ok", "create same-order child", f"st={st}")
        st, tag, rows, _detail = run(s, "alter table p72_m attach partition p72_m1 for values in (1)")
        check(st == "ok", "attach same-order partition", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_m2 (c int, b text, a int primary key)")
        check(st == "ok", "create shuffled-order child", f"st={st}")
        st, tag, rows, _detail = run(s, "alter table p72_m attach partition p72_m2 for values in (2)")
        check(st == "ok", "attach shuffled-order partition", f"st={st}")
        st, tag, rows, _detail = run(s, "insert into p72_m values (2, 'b', 20)")
        check(st == "ok" and tag == "INSERT 0 1", "seed shuffled leaf", f"tag={tag}")
        st, tag, rows, _detail = run(
            s,
            "insert into p72_m values (2, 'B', 21) on conflict (a) "
            "do update set b = excluded.b returning a, b, c",
        )
        check(
            st == "ok" and tag == "INSERT 0 1" and rows == [("2", "B", "20")],
            "remapped do update: only b changed, parent order returned",
            f"st={st} tag={tag} rows={rows}",
        )
        st, tag, rows, _detail = run(s, "select c, b, a from p72_m2")
        check(st == "ok" and rows == [("20", "B", "2")],
              "leaf stores its own column order", f"rows={rows}")

        # --- 13. multilevel partitioning ----------------------------
        st, tag, rows, _detail = run(
            s,
            "create table p72_ml (a int primary key, b text) partition by list (a)",
        )
        check(st == "ok", "create multilevel root", f"st={st}")
        st, tag, rows, _detail = run(
            s,
            "create table p72_ml_i partition of p72_ml for values in (1, 2) "
            "partition by list (b)",
        )
        check(st == "ok", "create subpartitioned intermediate", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_ml_x partition of p72_ml_i for values in ('x')")
        check(st == "ok", "create sub-leaf x", f"st={st}")
        st, tag, rows, _detail = run(s, "create table p72_ml_y partition of p72_ml_i for values in ('y')")
        check(st == "ok", "create sub-leaf y", f"st={st}")
        st, tag, rows, _detail = run(
            s,
            "insert into p72_ml values (1, 'x') on conflict (a) "
            "do update set b = excluded.b returning a, b",
        )
        check(st == "ok" and tag == "INSERT 0 1" and rows == [("1", "x")],
              "upsert through subpartitioned intermediate", f"rows={rows}")
        st, tag, rows, _detail = run(s, "select b from p72_ml_x")
        check(st == "ok" and rows == [("x",)], "row landed in the sub-leaf",
              f"rows={rows}")
        r = run(
            s,
            "insert into p72_ml values (1, 'x') on conflict (a) do update set b = 'y'",
        )
        check(
            r[0] == "error" and r[1] == "0A000"
            and r[3] == "The result tuple would appear in a different partition than the original tuple.",
            "second-level cross-partition move -> 0A000",
            f"r={r}",
        )

        # --- 14b. deterministic DO UPDATE: 21000 ---------------------
        # Two proposed rows conflicting with the same existing row;
        # the second DO UPDATE is rejected (PG19 cardinality).
        r = run(
            s,
            "insert into p72_pt values (2, 'x'), (2, 'y') on conflict (a) "
            "do update set b = excluded.b",
        )
        check(
            r[0] == "error" and r[1] == "21000",
            "second do update on same row -> 21000",
            f"r={r}",
        )
        st, tag, rows, _detail = run(s, "select a, b from p72_p1")
        check(st == "ok" and sorted(rows) == [("1", "uno"), ("2", "two")],
              "failed statement left the rows unchanged", f"rows={rows}")

        # --- 15. transactional rollback -----------------------------
        st, tag, rows, _detail = run(s, "begin")
        check(st == "ok", "begin", f"st={st}")
        st, tag, rows, _detail = run(
            s, "insert into p72_pt values (2, 'rb') on conflict (a) do nothing"
        )
        check(st == "ok" and tag == "INSERT 0 0", "upsert inside txn", f"tag={tag}")
        st, tag, rows, _detail = run(s, "rollback")
        check(st == "ok", "rollback", f"st={st}")
        st, tag, rows, _detail = run(s, "select b from p72_pt")
        check(st == "ok" and "rb" not in [r[0] for r in rows],
              "rolled-back upsert left no trace", f"rows={rows}")

        # --- cleanup ------------------------------------------------
        for t in ("p72_pt", "p72_m", "p72_ml"):
            st, tag, rows, _detail = run(s, f"drop table {t}")
            check(st == "ok", f"drop {t}", f"st={st}")

    finally:
        s.close()
    print(f"ALL {CHECKS} CHECKS PASSED")


if __name__ == "__main__":
    sys.exit(main())
