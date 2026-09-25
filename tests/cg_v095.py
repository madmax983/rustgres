#!/usr/bin/env python3
"""cg_v095.py — callgrind/DHAT workload driver for the v0.95 PG19-parity paths.

Assumes a rustgres server already running (under callgrind or DHAT) on 5434.
Exercises the v0.95 hot paths:
  1. parse_ident calls (parse_ident_parts: quoting/folding scanner)
  2. named-arg function calls (resolve_named_args)
  3. HAVING whole-expression grouping (structural GROUP BY match)
  4. degenerate grouping (is_degenerate_grouping)
  5. virtual pg_class self-row
  6. correlated derived-table queries (enclosing scope)
"""
import socket, struct, sys

PORT = 5434
N = int(sys.argv[1]) if len(sys.argv) > 1 else 60


def main():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=120)
    body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
    s.sendall(struct.pack("!i", len(body) + 4) + body)
    while True:
        h = s.recv(5)
        t, ln = h[0:1], struct.unpack("!I", h[1:5])[0]
        b = b""
        while len(b) < ln - 4:
            b += s.recv(ln - 4 - len(b))
        if t == b"Z":
            break

    def q(sql):
        s.sendall(b"Q" + struct.pack("!I", 4 + len(sql) + 1) + sql.encode() + b"\x00")
        while True:
            h = s.recv(5)
            if len(h) < 5:
                break
            t, ln = h[0:1], struct.unpack("!I", h[1:5])[0]
            b = b""
            while len(b) < ln - 4:
                chunk = s.recv(ln - 4 - len(b))
                if not chunk:
                    break
                b += chunk
            if t == b"Z":
                break

    q("create table cg95(c text, a int);")
    q("insert into cg95 select 'X' || (i % 7)::text, i from generate_series(1, 200) i;")
    q("create function cgadd(alpha int, beta int) returns int language sql as $$select alpha + beta$$;")

    for _ in range(N):
        # 1. parse_ident
        q("select parse_ident('\"Foo\".Bar.baz');")
        q("select parse_ident('a.b.c');")
        q("select parse_ident(qualname => '\"X\".y', strict => false);")
        # 2. named args
        q("select cgadd(beta => 2, alpha => 1);")
        q("select cgadd(10, beta => 5);")
        # 3. HAVING whole-expression grouping
        q("select lower(c), count(*) from cg95 group by lower(c);")
        q("select lower(c) from cg95 group by lower(c) having count(*) > 1;")
        # 4. degenerate grouping
        q("select 1 from cg95 where a > 0 having 1 < 2;")
        q("select count(*) from cg95 having 2 > 1;")
        # 5. pg_class self-row
        q("select oid, relname from pg_class where oid = 1259;")
        # 6. correlated derived table
        q("select x.a, (select max(t.a) from (select a from cg95 where a > x.a) t) from cg95 x limit 50;")
    s.close()
    print(f"workload done: {N} iterations")


if __name__ == "__main__":
    main()
