#!/usr/bin/env python3
"""v0.32 protocol tests: regexp set-returning functions (PG 19 semantics).

RED tests: regexp_matches() does not exist (42883); FROM-clause table
functions do not parse (42601); regexp_split_to_array mishandles patterns
that match empty ((?:), \\s*, '') and under-quotes array elements with
spaces.

Ground truth: PG REL_19_STABLE src/backend/utils/adt/regexp.c
(setup_regexp_matches degenerate-match rule, build_regexp_match_result,
build_regexp_split_result) and src/test/regress/expected/strings.out.
"""
import socket, struct, sys

PORT = 5433

def read_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c: raise RuntimeError("closed")
        d += c
    return d

def read_msg(s):
    t = read_exact(s, 1)
    (ln,) = struct.unpack("!i", read_exact(s, 4))
    return t, read_exact(s, ln - 4)

s = socket.create_connection(('127.0.0.1', PORT), timeout=10)
body = struct.pack("!i", 196608) + b"user\x00postgres\x00\x00"
s.sendall(struct.pack("!i", len(body) + 4) + body)
while True:
    t, p = read_msg(s)
    if t == b'Z': break

def do_sql(q):
    s.sendall(b'Q' + struct.pack("!i", len(q.encode()) + 5) + q.encode() + b'\x00')
    rows, err = [], None
    while True:
        t, p = read_msg(s)
        if t == b'D':
            (n,) = struct.unpack("!h", p[:2])
            pos, row = 2, []
            for _ in range(n):
                (ln,) = struct.unpack("!i", p[pos:pos+4]); pos += 4
                if ln == -1: row.append(None)
                else: row.append(p[pos:pos+ln].decode()); pos += ln
            rows.append(row)
        elif t == b'E':
            fields, pos = {}, 0
            while p[pos] != 0:
                code = chr(p[pos]); pos += 1
                end = p.index(b'\x00', pos)
                fields[code] = p[pos:end].decode(); pos = end + 1
            err = fields
        elif t == b'Z':
            break
    return rows, err

# (sql, expected_rows_or_None, expected_error_code_or_None)
TESTS = [
    # --- regexp_matches: SRF in SELECT list ---
    ("""SELECT regexp_matches('foobarbequebaz', $re$(bar)(beque)$re$)""",
     [['{bar,beque}']], None),
    ("""SELECT regexp_matches('foObARbEqUEbAz', $re$(bar)(beque)$re$, 'i')""",
     [['{bAR,bEqUE}']], None),
    ("""SELECT regexp_matches('foobarbequebazilbarfbonk', $re$(b[^b]+)(b[^b]+)$re$, 'g')""",
     [['{bar,beque}'], ['{bazil,barf}']], None),
    # empty capture group -> quoted ""
    ("""SELECT regexp_matches('foobarbequebaz', $re$(bar)(.*)(beque)$re$)""",
     [['{bar,"",beque}']], None),
    # no match -> zero rows
    ("""SELECT regexp_matches('foobarbequebaz', $re$(bar)(.+)(beque)$re$)""",
     [], None),
    # unmatched optional group -> NULL element
    ("""SELECT regexp_matches('foobarbequebaz', $re$(bar)(.+)?(beque)$re$)""",
     [['{bar,NULL,beque}']], None),
    # no groups -> whole match
    ("""SELECT regexp_matches('foobarbequebaz', $re$barbeque$re$)""",
     [['{barbeque}']], None),
    # zero-length ^ matches, multiline
    ("""SELECT regexp_matches('foo' || chr(10) || 'bar' || chr(10) || 'bequq' || chr(10) || 'baz', '^', 'mg')""",
     [['{""}'], ['{""}'], ['{""}'], ['{""}']], None),
    ("""SELECT regexp_matches('foo' || chr(10) || 'bar' || chr(10) || 'bequq' || chr(10) || 'baz', '$', 'mg')""",
     [['{""}'], ['{""}'], ['{""}'], ['{""}']], None),
    ("""SELECT regexp_matches('1' || chr(10) || '2' || chr(10) || '3' || chr(10) || '4' || chr(10), '^.?', 'mg')""",
     [['{1}'], ['{2}'], ['{3}'], ['{4}'], ['{""}']], None),
    ("""SELECT regexp_matches(chr(10) || '1' || chr(10) || '2' || chr(10) || '3' || chr(10) || '4', '.?$', 'mg')""",
     [['{""}'], ['{1}'], ['{""}'], ['{2}'], ['{""}'], ['{3}'], ['{""}'], ['{4}'], ['{""}']], None),
    # no match at all -> zero rows
    ("""SELECT regexp_matches('abc', 'x')""", [], None),
    # NULL input -> zero rows (strict SRF)
    ("""SELECT regexp_matches(NULL, 'x')""", [], None),
    # errors: bad flag, bad pattern
    ("""SELECT regexp_matches('foobarbequebaz', $re$(bar)(beque)$re$, 'gz')""",
     None, 'error'),
    ("""SELECT regexp_matches('foobarbequebaz', $re$(bar)(beque$re$)""",
     None, 'error'),
    # --- regexp_split_to_table: FROM-clause table function ---
    ("""SELECT foo, length(foo) FROM regexp_split_to_table('the quick brown fox jumps over the lazy dog', $re$\\s+$re$) AS foo""",
     [['the', '3'], ['quick', '5'], ['brown', '5'], ['fox', '3'], ['jumps', '5'],
      ['over', '4'], ['the', '3'], ['lazy', '4'], ['dog', '3']], None),
    ("""SELECT foo FROM regexp_split_to_table('the quick brown fox jumps over the lazy dog', $re$\\s*$re$) AS foo""",
     [[c] for c in 'thequickbrownfoxjumpsoverthelazydog'], None),
    ("""SELECT count(*) FROM regexp_split_to_table('the quick brown fox jumps over the lazy dog', '') AS foo""",
     [['43']], None),
    ("""SELECT foo FROM regexp_split_to_table('thE QUick bROWn FOx jUMPs ovEr The lazy dOG', 'e', 'i') AS foo""",
     [['th'], [' QUick bROWn FOx jUMPs ov'], ['r Th'], [' lazy dOG']], None),
    ("""SELECT foo FROM regexp_split_to_table('the quick brown fox jumps over the lazy dog', 'nomatch') AS foo""",
     [['the quick brown fox jumps over the lazy dog']], None),
    # 'g' flag rejected for split
    ("""SELECT * FROM regexp_split_to_table('a b', ' ', 'g') AS foo""",
     None, 'error'),
    # unknown table function
    ("""SELECT * FROM nosuchfunc('a') AS foo""", None, '42883'),
    # --- regexp_split_to_array: empty-match splitting (PG degenerate rule) ---
    ("""SELECT regexp_split_to_array('123456','(?:)')""",
     [['{1,2,3,4,5,6}']], None),
    ("""SELECT regexp_split_to_array('the quick brown fox jumps over the lazy dog', $re$\\s*$re$)""",
     [['{t,h,e,q,u,i,c,k,b,r,o,w,n,f,o,x,j,u,m,p,s,o,v,e,r,t,h,e,l,a,z,y,d,o,g}']], None),
    ("""SELECT regexp_split_to_array('the quick brown fox jumps over the lazy dog', '')""",
     [['{t,h,e," ",q,u,i,c,k," ",b,r,o,w,n," ",f,o,x," ",j,u,m,p,s," ",o,v,e,r," ",t,h,e," ",l,a,z,y," ",d,o,g}']], None),
    ("""SELECT regexp_split_to_array('the quick brown fox jumps over the lazy dog', 'nomatch')""",
     [['{"the quick brown fox jumps over the lazy dog"}']], None),
    ("""SELECT regexp_split_to_array('thE QUick bROWn FOx jUMPs ovEr The lazy dOG', 'e', 'i')""",
     [['{th," QUick bROWn FOx jUMPs ov","r Th"," lazy dOG"}']], None),
]

fails = 0
for i, (sql, exp_rows, exp_err) in enumerate(TESTS):
    rows, err = do_sql(sql)
    if exp_err == 'error':
        ok = err is not None
    elif exp_err:
        ok = err is not None and err.get('C') == exp_err
    else:
        ok = err is None and rows == exp_rows
    if not ok:
        fails += 1
        print(f"FAIL[{i}]: {sql[:80]}")
        print(f"   expected rows={exp_rows} err={exp_err}")
        print(f"   got      rows={rows} err={err.get('C') if err else None}")
print(f"{len(TESTS)-fails}/{len(TESTS)} passed")
sys.exit(1 if fails else 0)
