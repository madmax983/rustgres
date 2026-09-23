#!/usr/bin/env python3
r"""v0.67 protocol tests: numeric edge-case parity with PG19.

RED on the v0.66 base (713dfe9d), GREEN after v0.67.

1. test25 giant lcm hang: `lcm(9999 * 10^131068 + (10^131068 - 1), 2)`
   hung the server (>20s, wedging new connections) because
   BigUint::div_rem is bit-by-bit O(bits^2) and the numeric gcd/lcm
   Euclidean loop fed it a 435000-bit dividend divided by 2. PG19
   computes this in milliseconds (numeric_lcm -> div_var exact ->
   make_result) and raises 22003 only for the 131073-digit result.
   v0.67 adds the O(n) single-limb divisor fast path: 22003, fast.
2. test7 smallint: PG19 does NOT promote smallint+smallint to integer
   (pg_operator.dat oid 550, +(int2,int2) -> int2 via int2pl,
   src/backend/utils/adt/int.c raises 22003 "smallint out of range").
3. test18 exp: PG19's exp_var guard is |x| >= NUMERIC_MAX_RESULT_SCALE*3
   = 6000 (src/backend/utils/adt/numeric.c), so exp(1000) is a finite
   435-digit value; genuine overflow needs |x| >= 6000.
4. test21 1e200: PG19 round(float8) -> float8 via C rint (half to even,
   src/backend/utils/adt/float.c dround); numeric output is plain
   decimal (numeric_out -> get_str_from_var, never scientific).

Requires a running rustgres server on port 5433.
Run: python3 tests/protocol_test68.py
"""
import socket
import struct
import sys
import time

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
    list of first-column text values.
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

        # --- 1. giant lcm: 22003, fast (was a >20s hang) ---------------
        t0 = time.time()
        st, code, msg = run(
            s,
            "SELECT lcm(9999 * (10::numeric)^131068 + ((10::numeric)^131068 - 1), 2)",
        )
        dt = time.time() - t0
        check(
            st == "error" and code == "22003" and dt < 10,
            "giant lcm -> 22003 promptly",
            f"st={st} code={code} dt={dt:.1f}s",
        )
        # Server still responsive after the giant query.
        st, tag, rows = run(s, "SELECT 1")
        check(st == "ok" and rows == ["1"], "server responsive after giant lcm")

        # Big-but-legal gcd/lcm results (past i128, within 131072
        # digits) are values now, not spurious 22003.
        st, tag, rows = run(s, "SELECT gcd((10::numeric)^50, (10::numeric)^50)")
        check(
            st == "ok" and rows == ["1" + "0" * 50],
            "gcd(10^50, 10^50) = 10^50",
            f"rows={str(rows)[:60]}",
        )
        st, tag, rows = run(s, "SELECT length((lcm((10::numeric)^50, 3::numeric))::text)")
        check(st == "ok" and rows == ["51"], "lcm(10^50, 3) has 51 digits")

        # --- 2. smallint: no promotion, overflow is 22003 ---------------
        st, code, msg = run(s, "SELECT 30000::smallint + 30000::smallint")
        check(
            st == "error" and code == "22003",
            "smallint+smallint overflow -> 22003 (no PG19 promotion)",
            f"st={st} code={code}",
        )
        st, tag, rows = run(s, "SELECT 100::smallint + 200::smallint")
        check(st == "ok" and rows == ["300"], "smallint+smallint in range -> 300")

        # --- 3. exp: PG19's overflow guard is |x| >= 6000 ---------------
        st, tag, rows = run(s, "SELECT exp(1000::numeric)")
        check(
            st == "ok"
            and rows[0].startswith("1970071114017046993888879352243323125")
            and len(rows[0]) == 435,
            "exp(1000) finite 435-digit (PG19 guard is 6000)",
            f"st={st} rows={str(rows)[:60]}",
        )
        st, code, msg = run(s, "SELECT exp(10000::numeric)")
        check(
            st == "error" and code == "22003",
            "exp(10000) -> 22003",
            f"st={st} code={code}",
        )
        st, tag, rows = run(s, "SELECT exp(-10000::numeric)")
        check(
            st == "ok" and rows == ["0." + "0" * 1000],
            "exp(-10000) underflows to 0",
            f"rows={str(rows)[:60]}",
        )

        # --- 4. 1e200: float8 round -> float8; numeric plain decimal ----
        st, tag, rows = run(s, "SELECT round('1.2345678901234e200'::float8)")
        check(
            st == "ok" and rows == ["1.2345678901234e+200"],
            "round(1e200 float8) -> float8 scientific",
            f"rows={rows}",
        )
        st, tag, rows = run(s, "SELECT round('-1.2345678901234e200'::float8)")
        check(
            st == "ok" and rows == ["-1.2345678901234e+200"],
            "round(-1e200 float8) -> float8 scientific",
            f"rows={rows}",
        )
        # Half to even, like PG's dround (C rint).
        st, tag, rows = run(s, "SELECT round(2.5::float8)")
        check(st == "ok" and rows == ["2"], "round(2.5::float8) = 2 (half-even)")
        st, tag, rows = run(s, "SELECT round(3.5::float8)")
        check(st == "ok" and rows == ["4"], "round(3.5::float8) = 4 (half-even)")
        # Numeric round is untouched (half away from zero).
        st, tag, rows = run(s, "SELECT round(2.5::numeric)")
        check(st == "ok" and rows == ["3"], "round(2.5::numeric) = 3")
        # Numeric output is plain decimal (PG19 numeric_out), never
        # scientific.
        want200 = "1" + "0" * 200
        st, tag, rows = run(s, "SELECT '1e200'::numeric")
        check(st == "ok" and rows == [want200], "1e200 numeric literal plain decimal")
        st, tag, rows = run(s, "SELECT round('1e200'::numeric)")
        check(st == "ok" and rows == [want200], "round(1e200 numeric) plain decimal")
        st, tag, rows = run(s, "SELECT trunc('1e200'::numeric)")
        check(st == "ok" and rows == [want200], "trunc(1e200 numeric) plain decimal")
        st, tag, rows = run(s, "SELECT abs('-1e200'::numeric)")
        check(st == "ok" and rows == [want200], "abs(-1e200 numeric) plain decimal")

        print(f"\nv0.67 protocol: {CHECKS}/{CHECKS} passed")
    finally:
        s.close()


if __name__ == "__main__":
    main()
