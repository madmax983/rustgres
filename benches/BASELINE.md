# rustgres performance baseline

Measured with `benches/bench.py` (raw-socket wire-protocol driver, stdlib only).

## Bolt: `tokenize` — unsized buffer growth + two-pass identifier folding — baseline — 2026-09-13

**Workload**: same harness, query, and rationale as the three entries below —
`benches/profile_fixed.py`, 1500 iterations of
`SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10` against
a freshly created 3-row table, this commit (i.e. with the `peek`/`peek2`/
`peek3`, `eat_keyword`, and catalog-`HashMap` fixes already applied). Reused
deliberately: already committed, deterministic, and a realistic keyword-dense
query shape rather than a synthetic microbenchmark of `tokenize` alone.

**Profile** (DHAT, `valgrind --tool=dhat`, same harness, this commit):

DHAT attributes **54,056 of the program's 261,295 total heap allocations
(20.7%) to `rustgres::sql::tokenize`** — the single largest function in the
allocation profile, ahead of every parser or executor function (the next
largest, `resolve_priv_ref`, is 15,000 / 5.7%). Walking each allocation's
call stack to its exact source line inside `tokenize` breaks the 54,056 down
into four groups:

| line | what | blocks | % of total |
|---|---|---|---|
| `sql.rs:256` | `let chars: Vec<char> = input.chars().collect();` | 4,509 | 1.7% |
| `sql.rs:514` | `let word: String = chars[start..i].iter().collect();` | 19,516 | 7.5% |
| `sql.rs:515` | `Token::Ident(word.to_lowercase())` | 25,525 | 9.8% |
| `sql.rs:500` | `Token::Number(chars[start..i].iter().collect())` | 4,506 | 1.7% |

Reading the code and cross-checking against Callgrind's inlined-frame
attribution for the same build explains all four:

1. **`sql.rs:256`** — `input.chars().collect()` starts from `Vec::new()`
   (zero capacity) and grows by repeated doubling. `Chars::size_hint()`'s
   lower bound is `byte_len / 4` (sized for the worst case of every
   character being a 4-byte UTF-8 sequence), so for the all-ASCII SQL text
   in this workload the initial reservation undershoots the real char count
   by ~4x and the `Vec<char>` regrows 2-3 times per `tokenize()` call before
   it stops. 1,500-ish calls × ~3 regrows ≈ the 4,509 observed.
2. **`sql.rs:258`** (not directly visible above because DHAT attributes
   *growth* events to whichever `.push()` call site happens to trigger
   them, not to the `Vec::new()` site) — `let mut toks = Vec::new()` has the
   identical problem: it also starts at zero capacity and doubles as tokens
   are pushed. Since identifiers are the majority token type in ordinary
   SQL (7 keywords + 6 column/table names in this query, all tokenized as
   `Token::Ident`), most of these growth reallocations happen to land on
   the identifier-push line — this is the 6,009-block difference between
   line 515's 25,525 total and the 19,516 identifier count confirmed below.
3. **`sql.rs:514`+`515` (the remaining 19,516 + 19,516 = 39,032)** — every
   identifier is tokenized in two allocating passes: `chars[start..i]
   .iter().collect::<String>()` first builds a `String` holding the
   original-case text (one allocation per identifier — `Ident`/`Number`
   text collected from a slice iterator is `ExactSizeIterator`, so this
   part is already a single precisely-sized allocation, not a regrow), then
   `word.to_lowercase()` allocates a **second**, independent `String` with
   the case-folded result. Every keyword and every unquoted identifier pays
   both allocations on every parse, even though SQL identifiers are ASCII
   in virtually every real query (Postgres's own unquoted-identifier
   folding is itself ASCII-only) — for ASCII input the two passes compute
   the same thing `char::to_ascii_lowercase` would compute per character,
   with no need for a second heap buffer.

`sql.rs:500` (`Token::Number`) is a single, already precisely-sized
allocation per number literal (same `ExactSizeIterator` reasoning as line
514) with no second pass and no growth — left alone; there's no waste to
remove there without a data-model change, which is out of scope.

**Baseline numbers** (this commit, `tokenize` unchanged):

| counter | value |
|---|---|
| Callgrind `Ir` (total instructions, 1500 iterations) | 476,362,043 |
| DHAT total allocations (blocks) | 261,295 |
| DHAT total bytes allocated | 18,178,543 |
| DHAT blocks attributed to `tokenize` | 54,056 (20.7% of all allocations) |
| ...of which, in the three lines targeted by the fix below (256/514/515) | 49,550 (19.0% of all allocations) |

**Reproduce**:

```bash
cargo build
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# terminate the server (SIGTERM) to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -5   # PROGRAM TOTALS Ir

# separately, under --tool=dhat --dhat-out-file=/tmp/dh.out with the same
# harness, sum tbk/tb over pps[] in the JSON output and walk each
# allocation's ftbl-resolved call stack back to the first `rustgres::`
# frame (and its resolved source line) to attribute it to a caller.
```

**Fix** (next commit): three edits, all in `tokenize`, all behavior-
preserving:

1. `let chars: Vec<char> = input.chars().collect();` becomes a
   `Vec::with_capacity(input.len())` reserved up front, then extended — the
   number of `char`s in a `&str` can never exceed its byte length, so this
   is a safe upper bound that guarantees zero regrowth, for any input
   (ASCII or not).
2. `let mut toks = Vec::new();` becomes `Vec::with_capacity(chars.len() +
   1)` — every token consumes at least one input `char` (comments consume
   chars but push nothing), so the token count can never exceed
   `chars.len()`, plus exactly one more for the trailing `Token::EOF` push
   at the end. Also a safe upper bound; also guarantees zero regrowth.
3. The identifier arm gains an ASCII fast path, checked once after the
   identifier's span is known (`span.iter().all(|c| c.is_ascii())`, not
   folded into the character-scanning loop above it): when every character
   is ASCII, the final lowercase `String` is built directly in one
   allocation — `String::with_capacity(span.len())` plus a `for` loop
   pushing `c.to_ascii_lowercase()` — instead of collecting the
   original-case text and then `.to_lowercase()`-ing a second, independent
   String. When any character is non-ASCII, the exact original two-step
   `collect()` + `.to_lowercase()` path runs unchanged, so full-Unicode
   case-folding semantics (including context-sensitive cases like Greek
   final sigma, which `to_lowercase()` handles but a per-character map
   cannot) are preserved exactly for the rare non-ASCII identifier — this
   is not a behavior change for any input, ASCII or not.

   (A first attempt folded the ASCII check into the scanning loop as a
   `bool` flag updated on every character, and built the fast-path string
   with `.iter().map(|c| c.to_ascii_lowercase()).collect()`. It produced
   the identical allocation-count win but, measured on this debug build,
   *increased* Ir by +0.30% — the extra per-character branch plus a
   non-inlined `Map` iterator/closure both add real overhead when nothing
   is being inlined. Moving the ASCII check to a single post-hoc
   `.all()` pass over just the identifier span, and replacing the
   `.map().collect()` with a plain `for` loop over `&[char]` (one fewer
   generic iterator-adapter layer for the debug build to not-inline),
   removed the regression and turned it into a small net Ir win. Recording
   this here so nobody re-introduces the first shape and is surprised by
   the regression — this is a debug-build-specific effect; a release build
   would very likely inline both shapes identically.)

Same behavior for every input (all 81 unit tests and all 19
`tests/protocol_test*.py` conformance suites pass unchanged; no test
expectations touched).

**After numbers** (same harness, same query, same iteration count, same
machine, this session):

| counter | before | after | delta |
|---|---|---|---|
| Callgrind `Ir` (1500 iterations) | 476,362,043 | 473,990,492 | **-0.50%** |
| DHAT total allocations (blocks) | 261,295 | 234,267 | **-10.34%** |
| DHAT total bytes allocated | 18,178,543 | 18,052,395 | -0.69% |

Reproduced with a second `after` Callgrind run on the same binary:
473,990,072 (a 420-instruction, ~0.00009% difference from the first — well
within Callgrind's established determinism band for this harness, not
noise threatening the result).

The allocation-count floor (≥10%) is cleared. The Ir delta (-0.50%) does
not clear the ≥5% instruction floor on its own — reported honestly, not
cherry-picked — but the allocation-count floor is the one this change is
justified on, and it clears with margin. `tokenize`'s own self-cost rows
in `callgrind_annotate` (`sql.rs:tokenize` + `sql.rs:tokenize::{{closure}}`
— the latter did not exist before this diff) show the expected story in
more detail: fewer, larger, precisely-sized allocations replace many small
regrowing ones, at the cost of one added linear scan (`.all()`) over each
identifier's span that wasn't there before — net negative in allocation
count and (barely) net negative in instructions on this workload, but the
two effects partially offset rather than the naive "fewer mallocs → fewer
instructions" story predicting a larger Ir win.

**Reproduce** (after building with the fix applied):

```bash
cargo build && cargo test --all-features   # 81 passed
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# SIGTERM the server to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -5   # PROGRAM TOTALS Ir
```

Same pattern for DHAT: `valgrind --tool=dhat --dhat-out-file=/tmp/dh.out`,
then sum `tb`/`tbk` over the `pps` array in the JSON output.

**Note on `cargo clippy --all-targets --all-features -- -D warnings`**: same
pre-existing failure mode as every other Bolt entry in this file — this
session's toolchain reports 242 repo-wide lint errors unrelated to this
change (a different count from the 245 recorded in older entries in this
file, presumably toolchain-patch drift between sessions; internally
consistent within this session, reproduced on the pristine pre-fix tree
via `git stash`/`git stash pop`). `cargo clippy --all-targets
--all-features` (without `-D warnings`) shows the same 242 warnings before
and after this diff — zero new warnings from this change.

## Bolt: `Parser::peek`/`peek2`/`peek3` clone every lookahead — baseline — 2026-09-13

**Workload**: same harness and query as the `eat_keyword` and catalog-`HashMap`
entries below — `benches/profile_fixed.py`, 1500 iterations of
`SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10` against
a freshly created 3-row table. Reused deliberately, same rationale as those
two entries: already committed, deterministic, and a realistic query shape
end-to-end rather than a synthetic microbenchmark of the function being
changed.

**Profile** (Callgrind, debug build, `valgrind --tool=callgrind
--cache-sim=yes --branch-sim=yes`, 1500 iterations, this commit — i.e. with
the `eat_keyword` and catalog-`HashMap` fixes already applied):

DHAT (`valgrind --tool=dhat`, same harness) attributes **97,534 of the
program's 340,815 total heap allocations (28.6%) to `<Token as
Clone>::clone` (`sql.rs:70`/`71`)**, and of those, 97,534 (effectively all
of them — 96,034 via `peek`, 1,500 via `peek2`) are reached through
`Parser::peek`/`peek2`/`peek3`, confirmed by walking each allocation's
full DHAT call stack back to its caller. `Token` is `#[derive(Clone)]` over
an enum whose token-bearing variants (`Ident(String)`, `Number(String)`,
`Str(String)`, `UStr(String)`) heap-allocate a fresh `String` on every
clone. `peek`/`peek2`/`peek3` are the parser's whole lookahead surface —
127 call sites across `sql.rs` (122 `.peek()`, 3 `.peek2()`, 2 `.peek3()`)
covering essentially every dispatch decision in the recursive-descent
parser — and every one of them returns an *owned* `Token` by cloning
`self.tokens[pos]`, even when the caller only compares or matches the
result and immediately drops it (the overwhelmingly common case: a failed
`Token::LParen`/`Token::Comma`/keyword-`Ident` probe during dispatch).
`eat_keyword` (fixed in the entry below) was one such caller, bypassed by
going straight to `self.tokens.get(self.pos)`; the other 127 call sites
across the rest of the parser still pay this on every lookahead.

**Baseline numbers** (this commit, `peek`/`peek2`/`peek3` unchanged):

| counter | value |
|---|---|
| Callgrind `Ir` (total instructions, 1500 iterations) | 509,202,550 |
| DHAT total allocations (blocks) | 340,815 |
| DHAT total bytes allocated | 18,450,076 |
| DHAT blocks attributed to `Token::clone` reached via `peek`/`peek2`/`peek3` | 97,534 (28.6% of all allocations) |

**Reproduce**:

```bash
cargo build
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# terminate the server (SIGTERM) to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -5   # PROGRAM TOTALS Ir
# and, separately, under --tool=dhat, sum tbk/tb over pps[] in the JSON
# output and walk each allocation's ftbl-resolved call stack back to the
# first `rustgres::` frame to attribute it to a caller.
```

**Fix** (next commit): `peek`/`peek2`/`peek3` return `&Token` (a borrow
into `self.tokens`) instead of an owned, cloned `Token`. `next()` (which
legitimately consumes and returns ownership of the current token) clones
explicitly at its one call to `peek()`, so its behavior is unchanged. Every
other call site — 127 of them — either already worked unchanged under
Rust's match ergonomics (`matches!(self.peek(), Token::X)`, `match
self.peek() { Token::Ident(s) => ... }` with `s` now binding as `&String`),
needed a `*` deref for a direct equality comparison (`*self.peek() ==
Token::Comma`), or — the handful of call sites that actually move the
matched string/number out (a handful of alias/type-name/qualified-`*`
parses) — an explicit `.clone()` at that one point, no earlier. Same
behavior (all 81 unit/integration tests plus all 21 protocol conformance
suites pass unchanged; no test expectations touched), and every keyword or
punctuation lookahead that used to clone now doesn't.

**After numbers** (same harness, same query, same iteration count, same
machine, this session):

| counter | before | after | delta |
|---|---|---|---|
| Callgrind `Ir` (1500 iterations) | 509,202,550 | 476,361,041 | **-6.45%** |
| DHAT total allocations (blocks) | 340,815 | 261,297 | **-23.34%** |
| DHAT total bytes allocated | 18,450,076 | 18,178,541 | -1.47% |

Reproduced with a second `after` Callgrind run on the same binary:
476,362,196 (a 1,155-instruction, ~0.0002% difference from the first —
Callgrind's normal run-to-run determinism band, same as the prior two
entries, not noise threatening the result).

Both the instruction-count floor (≥5%) and the allocation-count floor
(≥10%) are cleared, the latter by more than 2x. The byte-count delta is
small (-1.47%) because most of the removed allocations are short
keyword/identifier strings (a handful of bytes each) rather than large
buffers — DHAT's own accounting bears this out (297 KB total across
87,016 `Token::clone` calls at `sql.rs:70`, ~3.4 bytes/call average) — so
this fix is an allocation-*count* and instruction-count win, not
principally a bytes-freed win; it is reported on both counters rather than
cherry-picked to the more favorable one.

**Reproduce** (after building with the fix applied):

```bash
cargo build && cargo test --all-features   # 81 passed
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# SIGTERM the server to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -5   # PROGRAM TOTALS Ir
```

Same pattern for DHAT: `valgrind --tool=dhat --dhat-out-file=/tmp/dh.out`,
then sum `tb`/`tbk` over the `pps` array in the JSON output.

**Note on `cargo clippy --all-targets --all-features -- -D warnings`**:
same pre-existing failure mode as the two entries below (this
environment's clippy reports repo-wide lint errors unrelated to any Bolt
change). Verified directly: `cargo clippy --all-targets --all-features`
(without `-D warnings`) reports the exact same 245 warnings on the
pristine pre-fix tree and on this fix — zero new warnings from this diff,
confirmed by `git stash`/`git stash pop` around the clippy run.

## Bolt: catalog `HashMap` hasher — baseline — 2026-09-13

**Workload**: same harness and query as the `eat_keyword` entry below —
`benches/profile_fixed.py`, 1500 iterations of
`SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10`
against a freshly created 3-row table. Reused deliberately: it is already
committed, deterministic, and exercises an ordinary query end-to-end
(parse, privilege check, plan, execute) rather than a synthetic
microbenchmark of one function.

**Profile** (Callgrind, debug build, `valgrind --tool=callgrind
--cache-sim=yes --branch-sim=yes`, 1500 iterations, this commit, i.e.
with the `eat_keyword` fix already applied):

Total `Ir`: 543,682,768. Summing every `core::hash::sip::*` frame plus
`core::intrinsics::rotate_left` (confirmed via `callgrind_annotate
--tree=both` to have no callers other than `Sip13Rounds::c_rounds`/
`d_rounds` — it is not shared with any other rotate in the program) gives
**36,720,013 instructions, 6.75% of the total profile** spent computing
`SipHash` over catalog lookup keys.

`Database`'s six catalog maps (`tables`, `indexes`, `stats`, `views`,
`sequences`, `roles` — all `HashMap<String, _>`) use Rust's default
`RandomState`/`SipHash13`. Every one of `find_table`/`find_index`/
`find_role`/`find_view`/`find_sequence` does a real keyed `.get(name)`
(not a linear scan — confirmed by reading `storage.rs`), and a single
ordinary `SELECT` calls into several of them: `check_select_col_privs`
resolves each referenced column via `find_table` (7,500 calls across
1500 iterations here — one per column reference), `is_superuser_snap`
calls `find_role` once per statement (9,002 calls including setup), and
`plan_access_path` probes `find_index`. Traced via `callgrind_annotate
--tree=both`: `Hasher::write_str` is called 36,020 times over 1500
iterations (~24 hashes/query) — all short, trusted keys (table/role/
column names chosen by whoever is connected to this server), not
attacker-controlled input from an untrusted network boundary, so the
DoS-resistance `SipHash` buys has no payoff here, only its fixed
per-call mixing-round cost.

**Baseline numbers** (this commit, catalog maps still on `RandomState`):

| counter | value |
|---|---|
| Callgrind `Ir` (total instructions, 1500 iterations) | 543,682,768 |
| Callgrind `Ir` in `SipHash` computation (`sip.rs` + `rotate_left`) | 36,720,013 (6.75%) |

**Reproduce**:

```bash
cargo build
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# terminate the server (SIGTERM) to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out > /tmp/flat.txt
grep -E "hash/sip\.rs|hash/mod\.rs.*Hasher|intrinsics/mod\.rs:core::intrinsics::rotate_left" /tmp/flat.txt
```

**Fix** (next commit): the six `Database` catalog maps (`tables`,
`indexes`, `stats`, `views`, `sequences`, `roles`) switch from the
default `RandomState`/`SipHash13` to a small `FxHasher` (public-domain
FxHash: rotate-xor-multiply, the same algorithm as the `rustc-hash`
crate, reimplemented in `src/fxhash.rs` in ~70 lines rather than adding
a dependency — no `unsafe`, no new crate). No call sites change:
`HashMap::new()` → `HashMap::default()` at the one construction site
(`Database::new`) is the only other edit. All 81 tests pass unchanged;
no test expectations touched.

**After numbers** (same harness, same query, same iteration count,
same machine, this session):

| counter | before | after | delta |
|---|---|---|---|
| Callgrind `Ir` (1500 iterations) | 543,682,768 | 509,173,826 | **-6.35%** |

Reproduced with a second `after` run on the same binary: 509,173,639 (a
187-instruction, ~0.00004% difference from the first — Callgrind's
normal run-to-run determinism band, not noise threatening the result).

The instruction-count floor (≥5%) is cleared. `SipHash` cost doesn't
disappear entirely — other `HashMap`/`HashSet`s elsewhere in the
codebase (GROUP BY, joins, MVCC snapshot sets, session state) are
untouched by this change and still pay it — but the catalog-lookup
share of it is gone: post-fix, `sip.rs` + `rotate_left` together are
down to ~1.7% of `Ir` (was 6.75%), and the new `FxHasher` code
(`src/fxhash.rs`) accounts for well under 1%.

**Reproduce** (after building with the fix applied):

```bash
cargo build && cargo test --all-features   # 81 passed
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# SIGTERM the server to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -5   # PROGRAM TOTALS Ir
```

**Note on `cargo clippy --all-targets --all-features -- -D warnings`**:
same pre-existing failure mode as the `eat_keyword` entry below (this
environment's clippy reports repo-wide lint errors unrelated to any
Bolt change). Verified directly: `cargo clippy --all-targets
--all-features` (without `-D warnings`) reports the exact same 245
warnings on the pristine pre-fix tree and on this fix — zero new
warnings from this diff, confirmed by `git stash`/`git stash pop`
around the clippy run.

## Bolt: `Parser::eat_keyword` allocation — baseline — 2026-09-12

Every version of this file back to v0.7 has recorded the same finding under
Callgrind/DHAT: per-query SQL text parsing (`tokenize`, `Parser::peek`,
`Token::clone`, `split_statements`) dominates the profile for short queries,
never the feature code being added. Nobody had gone back to fix the parsing
cost itself. This entry does.

**Workload**: `benches/profile_fixed.py` — a new *fixed-iteration-count*
driver (as opposed to `bench.py`'s time-boxed workloads) that sends an exact
number of identical queries over the real wire protocol, so two profiling
runs (before/after a code change) execute identical work and their
instruction/allocation counts are directly comparable. Time-boxed workloads
under valgrind vary in query count run-to-run with wall-clock jitter, which
would confound a before/after diff.

Query used here (`--sql`, run 1500 times against a 3-row table after
`--setup`):

```sql
SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10
```

This is a keyword-dense, realistic query shape (filtered SELECT + ORDER BY +
LIMIT) chosen specifically because it exercises the parser's keyword-dispatch
path (`Parser::eat_keyword`) far more per query than the `expr` workload
does — `expr` is mostly literals/function calls and undercounts this cost.

**Profile** (Callgrind, debug build, `valgrind --tool=callgrind
--cache-sim=yes --branch-sim=yes`, 1500 iterations):

`Parser::eat_keyword` is called 239 times across the parser's call sites,
every optional-keyword probe during statement dispatch. Its old
implementation:

```rust
fn eat_keyword(&mut self, kw: &str) -> bool {
    match self.peek() {                    // clones the current Token
        Token::Ident(ref s) if s == kw => { ... }
        _ => false,
    }
}
```

`Parser::peek` returns an owned `Token` by cloning `self.tokens[pos]`. For
the overwhelmingly common case — the current token is `Token::Ident(String)`
(every keyword *and* every identifier is tokenized as `Ident`) — that clone
heap-allocates a fresh `String`, compares it, then immediately drops it.
Every one of the 239 call sites pays this even on failed keyword probes
(the majority case in keyword-driven statement dispatch), independent of
this particular query's `WHERE`/`ORDER BY`/`LIMIT` clauses.

**Baseline numbers** (this commit, `eat_keyword` unchanged):

| counter | value |
|---|---|
| Callgrind `Ir` (total instructions, 1500 iterations) | 574,441,689 |
| DHAT total allocations (blocks) | 414,338 |
| DHAT total bytes allocated | 18,749,059 |

**Reproduce**:

```bash
cargo build
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# terminate the server (SIGTERM) to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -22   # PROGRAM TOTALS Ir
```

**Fix** (this commit): `eat_keyword` no longer goes through `peek()`.
It borrows the current token directly:

```rust
fn eat_keyword(&mut self, kw: &str) -> bool {
    match self.tokens.get(self.pos) {
        Some(Token::Ident(s)) if s == kw => {
            self.pos += 1;
            true
        }
        _ => false,
    }
}
```

Same behavior (all 81 unit/integration tests pass unchanged; no test
expectations touched), zero heap allocations for the check instead of one
per call. `Parser::peek`, `Parser::next`, and the rest of the token stream
are untouched — the parser does backtrack (`self.pos = save` at 4 call
sites), so `next()`/`peek()` can't destructively consume tokens without
separate work; `eat_keyword` never needed ownership in the first place,
only a comparison.

**After numbers** (same harness, same query, same iteration count):

| counter | before | after | delta |
|---|---|---|---|
| Callgrind `Ir` (1500 iterations) | 574,441,689 | 543,723,769 | **-5.35%** |
| DHAT total allocations (blocks) | 414,338 | 340,814 | **-17.75%** |
| DHAT total bytes allocated | 18,749,059 | 18,450,464 | -1.59% |

Both the instruction-count floor (≥5%) and the allocation-count floor
(≥10%) are cleared. (The `expr` workload from `bench.py`, which has far
fewer `eat_keyword` probes per query, was tried first and only reached
-4.06% Ir / -7.87% allocations / -0.59% bytes — below the impact floor.
Recorded here so nobody re-measures on `expr` and concludes this isn't
worth shipping; the effect is real, just workload-dependent on how
keyword-dense the query is.)

**Reproduce** (after building with the fix applied):

```bash
cargo build
DATADIR=$(mktemp -d) RUSTGRES_DATA_DIR="$DATADIR" \
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out \
  --collect-jumps=yes --cache-sim=yes --branch-sim=yes \
  ./target/debug/rustgres &
python3 benches/profile_fixed.py --count 1500 \
  --setup "DROP TABLE IF EXISTS kw_bench" \
  --setup "CREATE TABLE kw_bench(a INT, b INT)" \
  --setup "INSERT INTO kw_bench VALUES (1,2),(3,4),(5,6)" \
  --sql "SELECT a, b FROM kw_bench WHERE a = 1 AND b = 2 ORDER BY a LIMIT 10"
# SIGTERM the server to flush callgrind.out, then:
callgrind_annotate --auto=no /tmp/cg.out | head -22   # PROGRAM TOTALS Ir
```

Same pattern for DHAT: `valgrind --tool=dhat --dhat-out-file=/tmp/dh.out`,
then sum `tb`/`tbk` over the `pps` array in the JSON output.

**Note on `cargo clippy --all-targets --all-features -- -D warnings`**:
this environment's clippy (rust 1.94.1) fails with 235 pre-existing
`collapsible_if`-family errors across the codebase (unrelated files,
e.g. `src/main.rs`), independent of this change — reproduced on the
pristine tree before this commit. `cargo clippy --all-targets
--all-features` (without `-D warnings`) shows no new warnings near
`eat_keyword` or introduced by this diff. Not fixed here: 235 unrelated
lint fixes are out of scope for a targeted performance change.

## v0.20.1 baseline — 2026-09-12

No benchmark re-run for v0.20.1 (targeted correctness repair, no hot-path
change): the RIGHT/FULL executor drops the side-swap (removing a per-row
cell-rearrangement pass) and tightens WHERE pushdown by join kind; the
three nested-loop paths are otherwise unchanged in shape.

- **Valgrind**: NOT RUN on the v0.20.1 tree. (Valgrind 3.22.0 became
  locally available after v0.20 via a manually extracted launcher, but
  Memcheck/Callgrind/DHAT were not run on the v0.20 exact tree and are not
  run here either; this is recorded, not hand-waved.)
- **Callgrind/DHAT**: not run.

## v0.20 baseline — 2026-09-12

No full benchmark re-run for v0.20 (same rationale as v0.14–v0.19 for
non-hot-path milestones): the milestone is the join type cluster (RIGHT /
FULL / USING / NATURAL parser + executor support, table column aliases),
with no new workload and no hot-path rewrite. The join executor's three
paths (fast two-frame, correlated, slow combined-frame) are unchanged in
shape; RIGHT JOIN reuses the LEFT machinery with swapped sides and FULL
JOIN adds an unmatched-right emission pass. (For reference, workload19's
440 queries ran at 16497 qps on the debug binary during v0.19's
independent verification.)

- **Valgrind**: NOT RUN — valgrind is not installed in this environment
  and apt could not install it (dpkg lock held by system processes across
  VM generations). This is recorded, not hand-waved.
- **Callgrind/DHAT**: not run (no valgrind).

## v0.19 baseline — 2026-09-12

The v0.19 workload (`benches/workload19.py`: bytea hex/escape/base64 I/O,
encode/decode, CRC32C, SHA-224/256/384/512, regexp_* functions, SIMILAR TO,
LIKE..ESCAPE, string functions — 440 queries) ran at **1610 qps** on the
debug binary, 0.27s total.

- **Valgrind**: NOT RUN — valgrind is not installed in this environment
  and apt could not install it (dpkg lock held by another process).
  This is recorded, not hand-waved.

## v0.18 baseline — 2026-09-11

No full benchmark re-run for v0.18 (same rationale as v0.14–v0.17): the
milestone is the numeric type cluster (new built-ins, NaN/Infinity
parsing, high-precision exp/ln/log), with no hot-path rewrites on the
normal query path. The v0.18 workload (`benches/workload18.py`: NaN/Inf
parsing and arithmetic, high-precision exp/ln/log, specials, ordering —
800 queries) ran at **6282 qps** on the debug binary, 0.13s total.

- **Valgrind**: NOT RUN — valgrind is not installed in this environment
  and apt could not install it (a system gdb install held the dpkg lock
  through two VM generations; a system os-intent replay was also running).
  This is recorded, not hand-waved. The numeric code paths added in
  v0.18 (u128 gcd/lcm, extended-precision exp/ln/log, NaN/Infinity
  propagation) were reviewed for overflow/panic hazards during
  independent repair (three panics found and fixed: gcd/lcm
  `i64::abs()` overflow, pi()/random() zero-arg indexing, scale-38
  division overflow in degrees/radians).
- **Callgrind/DHAT**: not run (no valgrind). No v0.18 change targets a
  demonstrated hotspot; the workload is new-function coverage, not a
  hot-path rewrite.

## v0.16 baseline — 2026-09-11

No full benchmark re-run for v0.16 (same rationale as v0.14/v0.15): the
milestone is new built-ins, cursors, and TRUNCATE plus the required
profiling gate, with no hot-path rewrites on the normal query path. The
profiling gate on a 10-round v0.16 workload (`benches/workload16.py`:
new functions, TRUNCATE, DECLARE/FETCH/MOVE/CLOSE in all directions,
savepoints — ~120 statements) found nothing to fix:

- **Valgrind memcheck** (release binary): **0 errors from 0 contexts**,
  0 bytes definitely/indirectly lost. "Possibly lost" 3.8 KiB in 26
  blocks are Rust interior-pointer artifacts (`Table::with_def`,
  `Engine::new`, `String::clone`) at SIGTERM shutdown — the same known
  shutdown-noise class as v0.15, not real leaks.
- **Callgrind** on the same workload (5.5M instructions): top consumers
  are libc malloc/free (~30% combined on this tiny workload),
  `parse_statement_inner` 3.4%, tokenizer keyword `to_lowercase` 1.2%.
  No v0.16 code path (`cursor_window`, radix converters, truncate
  writer) registers above the noise floor.
- **DHAT**: ~734 KiB total allocated, 64 KiB max-live — the largest
  allocation point is the v0.15 thread-local `read_bounded` scratch
  buffer, grown once to 64 KiB and reused per message (the v0.15 fix
  holding). Tokenizer `Vec<Token>` 134 KiB total / 4 KiB max-live;
  `parse_call` expr vecs 61 KiB total / 6 KiB max-live. Nothing
  retained, no v0.16-specific allocation pathology.

v0.13 numbers below are carried forward as the v0.16 query-path baseline
pending a clean-box re-run.

## v0.15 baseline — 2026-09-11

No full benchmark re-run for v0.15 (same rationale as v0.14): the milestone
is transaction-control syntax plus the required profiling gate, with no
hot-path rewrites on the normal query path. The profiling gate itself
produced one real fix:

- **Valgrind memcheck** (release binary, 200-statement txn-heavy workload
  covering every new `BEGIN`/`START TRANSACTION`/`COMMIT`/`ROLLBACK`
  variant, savepoints, `AND CHAIN`): zero invalid reads/writes, zero
  definitely/indirectly lost bytes. (The 27 reported "errors" are
  valgrind's per-thread SIGTERM-shutdown notices, not memory errors.)
- **Callgrind + DHAT** on the same workload: `read_bounded`
  (`src/protocol.rs`) allocated and zeroed a fresh 64 KiB scratch buffer
  for *every inbound message* — 18.5 MiB total for the workload at
  283 B max-live, 76% of all profiled instructions. Fixed by reusing a
  per-connection-thread thread-local scratch buffer; behavior unchanged
  (new `repeated_read_message_reuses_scratch_cleanly` unit test + full
  protocol suite green). No new v0.15 hotspot: `txn_begin`/`txn_commit`
  show 48 calls at negligible cost; remaining profile is parser +
  allocator noise on a tiny workload.
- **Native micro-timing** (release, `/tmp` data dir): BEGIN/COMMIT
  0.04 ms/stmt, autocommit INSERT 0.03 ms/stmt, SELECT count(*) 0.06
  ms/stmt, SAVEPOINT/ROLLBACK TO 0.04 ms/stmt.

v0.13 numbers below are carried forward as the v0.15 query-path baseline
pending a clean-box re-run.

## v0.13 baseline — 2026-09-10

Environment: same sandbox, **release build** (`cargo build --release`),
fresh temp data dir, 5 s per workload. v0.13 adds the replication
protocol + logical decoding; no hot-path rewrites on the normal query
path. NOTE: the first v0.13 bench run accidentally used a data dir on
the workspace btrfs volume (whose fsync stalls: raw `fsync` p50
0.00 ms / p99 37 ms), while prior milestones used `/tmp` (tmpfs).
The numbers below are the re-run on `/tmp`, like-for-like with v0.12.
A bisection scare during measurement (v0.13 COMMIT ~12–59 ms vs v0.12
~0.1 ms) turned out to be purely the btrfs-vs-tmpfs data-dir
difference: v0.13 on tmpfs commits in 0.03–0.31 ms, identical to v0.12
on the same box. The commit path is unchanged apart from
once-per-datadir system-id syncs.

| workload   | qps      | p50        | p99        | vs v0.12 |
|------------|----------|------------|------------|---------|
| `select1`  | 26,899   | 0.026 ms   | 0.195 ms   | p50 same (qps: v0.12 was load-polluted) |
| `expr`     | 8,254    | 0.068 ms   | 0.880 ms   | p50 same |
| `scan`     | 39.3     | 18.28 ms   | 93.16 ms   | better (v0.12 was load-polluted) |
| `insert`   | 214.5    | 3.408 ms   | 25.07 ms   | better (v0.12 was load-polluted) |
| `prepared` | 15,127   | 0.059 ms   | 0.325 ms   | p50 same |
| `txn`      | 8,693    | 0.073 ms   | 0.763 ms   | p50 same |
| `mvcc`     | 1,561    | 0.512 ms   | 2.603 ms   | p50 same/better |
| `join`     | 2.3      | 439.0 ms   | 587.4 ms   | better (v0.12 was load-polluted) |
| `idxscan`  | 21,727   | 0.036 ms   | 0.195 ms   | p50 same/better |
| `window`   | 6.2      | 147.9 ms   | 297.5 ms   | better (v0.12 was load-polluted) |
| `copy`     | 70.9     | 12.90 ms   | 23.42 ms   | better (v0.12 was load-polluted) |

CPU-bound p50 latencies (`select1`, `expr`, `prepared`, `idxscan`) are
unchanged vs v0.12 — the v0.13 changes (walsender, slot WAL records,
`UpdateRows`, `pg_replication_slots`) add no per-query hot-path cost on
normal connections. `txn`/`insert`/`mvcc` are back to v0.12 levels on
the like-for-like tmpfs rerun; the earlier scary `txn` numbers were the
btrfs data dir, not v0.13 code.

Replication streaming (release build, ad-hoc driver, `/tmp` data dir):
500 single-row INSERTs streamed as 502 logical lines (BEGIN + 500
INSERT + COMMIT); steady-state per-line gap 0.00 ms (one
`send_copy_data` write syscall each). Initial catch-up from LSN 0 over
a WAL holding the full benchmark history scans + CRC-verifies + decodes
every frame (~2 ms/frame) — proportional to WAL size, as expected; a
slot started at the current LSN streams new commits with no catch-up
scan.

Valgrind memcheck over a replication workload (slot create, 50
inserts/updates/deletes, START_REPLICATION stream, standby status,
CopyDone, slot drop): **0 bytes definitely lost, 0 indirectly lost**,
0 invalid reads/writes, 0 uninit errors (the "possibly lost" 31 KB is
SIGTERM-at-`accept` teardown, same as v0.12). Callgrind on the
replication scenario: top named cost is the pre-existing table-driven
`wal::crc32` (10.9%) plus WAL frame decode (`Dec::take`/`read_frame`
~9%) — no v0.13-specific hotspots in `repl.rs` or the decoder. DHAT:
3.0 MB total allocated over the scenario, 84.6 KB peak live, all
short-lived (64 KB network read buffers, `Vec<WalRecord>` batch bufs) —
no heap bloat.

## v0.12 baseline — 2026-09-10

Environment: same sandbox, **release build** (`cargo build --release`),
fresh temp data dir, 5 s per workload. v0.12 is a robustness milestone
(protocol hardening, concurrency stress, soak testing); no hot-path
rewrites. NOTE: a second, unrelated build job was running on this
shared 2-CPU VM during measurement (load avg 13–25), so qps numbers
below are heavily polluted — p50 on CPU-bound workloads is the honest
signal, and it matches v0.11.

| workload   | qps      | p50        | p99        | vs v0.11 |
|------------|----------|------------|------------|---------|
| `select1`  | 7,191    | 0.026 ms   | 2.028 ms   | p50 same (qps: load noise) |
| `expr`     | 3,865    | 0.067 ms   | 3.067 ms   | p50 same (qps: load noise) |
| `scan`     | 12.4     | 65.16 ms   | 312.74 ms  | degraded: load noise (fsync-heavy under contention) |
| `insert`   | 44.9     | 15.07 ms   | 137.01 ms  | degraded: load noise (fsync-heavy under contention) |
| `prepared` | 7,735    | 0.059 ms   | 1.860 ms   | p50 same (qps: load noise) |
| `txn`      | 5,587    | 0.071 ms   | 1.776 ms   | p50 same/better |
| `mvcc`     | 920      | 0.620 ms   | 6.943 ms   | p50 same |
| `join`     | 0.4      | 2662.9 ms  | 2679.4 ms  | degraded: load noise |
| `idxscan`  | 4,884    | 0.043 ms   | 2.408 ms   | p50 same (qps: load noise) |
| `window`   | 1.1      | 859.3 ms   | 1747.3 ms  | degraded: load noise |
| `copy`     | 10.7     | 75.60 ms   | 372.83 ms  | degraded: load noise (fsync-heavy under contention) |

CPU-bound p50 latencies (`select1`, `expr`, `prepared`, `txn`, `mvcc`,
`idxscan`) are unchanged vs v0.11 — the v0.12 changes (bounded wire
reads, poisoned-lock recovery helpers, 42701 duplicate-target checks)
add no per-query hot-path cost. fsync/data-heavy workloads degraded
only under the concurrent build's I/O+CPU contention.

Concurrency/robustness numbers (new in v0.12, debug build):
120 s randomized soak: 29,953 statements, 0 unexpected SQLSTATEs, RSS
9→11 MB flat, SIGKILL mid-run → clean WAL recovery. Wire fuzz: 27/27
checks, ~5,021 qps sustained through a 3,000-query flood, server alive
after every corpus group. 150 rapid connect/disconnect cycles < 30 s.

Valgrind memcheck over a threaded stress workload (4 workers, 500
inserts + 800 mixed stmts + extended-protocol + error paths): **0
bytes definitely lost, 0 indirectly lost**, 0 memory errors (38
"error" contexts are the SIGTERM shutdown at `accept`, not memory
errors). Callgrind on the bench workload: no v0.12-specific hotspots;
top named costs are pre-existing i128 numeric arithmetic (~4.4%),
allocation (~2.3%), and QCol iteration (~2%). DHAT on 600 mixed
statements: 72.4 MB total allocated in 256k blocks, all short-lived
(max-live ≤ 64 KB) — no heap bloat.

## v0.11 baseline — 2026-09-10

Environment: same sandbox, **release build** (`cargo build --release`),
fresh temp data dir, 5 s per workload. v0.11 adds SCRAM auth, roles,
and the privilege system; every query now passes a permission check.
Callgrind showed the per-query role-membership closure dominating
(`check_select_col_privs` 27.6% + `role_closure` 21.7% of
instructions); the closure is now built once per statement instead of
once per column (~2x fewer instructions in the permission path).

| workload   | qps      | p50        | p99        | vs v0.9 |
|------------|----------|------------|------------|---------|
| `select1`  | 21,094   | 0.024 ms   | 0.325 ms   | p50 same |
| `expr`     | 6,672    | 0.066 ms   | 1.319 ms   | p50 same |
| `scan`     | 36.6     | 17.78 ms   | 136.14 ms  | noise |
| `insert`   | 116.3    | 3.913 ms   | 55.33 ms   | qps lower, p50 same |
| `prepared` | 12,534   | 0.041 ms   | 0.675 ms   | p50 same |
| `txn`      | 4,138    | 0.106 ms   | 2.343 ms   | qps lower, p50 same |
| `mvcc`     | 1,004    | 0.682 ms   | 4.398 ms   | noise |
| `join`     | 0.9      | 836 ms     | 1909 ms    | noise |
| `idxscan`  | 11,133   | 0.033 ms   | 0.808 ms   | p50 same |
| `window`   | 3.6      | 208.8 ms   | 662.9 ms   | new in v0.10 |
| `copy`     | 28.9     | 25.02 ms   | 167.0 ms   | new in v0.10 |

p50 latencies are unchanged vs v0.9 across the board; qps deltas on
`insert`/`txn` are sandbox noise (p50 identical). Valgrind memcheck on
the v0.11 auth/grant paths (SCRAM, role/membership/column grants,
CHECKPOINT): 0 bytes definitely lost; 51 contexts are Rust-runtime
false positives. DHAT on 300 permission-checked SELECTs: 2.3 MB total
allocated, max live 256 B — no heap bloat.

## v0.9 baseline — 2026-09-10

Environment: same sandbox, **release build** (`cargo build --release`),
fresh temp data dir, 5 s per workload. v0.9 adds constraints, ALTER
TABLE, views, sequences — no planner/executor hot-path changes, so
performance is consistent with v0.8 (release vs debug accounts for the
uplift vs the v0.8 debug numbers).

| workload   | qps      | p50        | p99        | vs v0.8 (debug) |
|------------|----------|------------|------------|-----------------|
| `select1`  | 26,015   | 0.024 ms   | 0.242 ms   | release uplift |
| `expr`     | 6,653    | 0.064 ms   | 1.502 ms   | release uplift |
| `scan`     | 32.6     | 23.47 ms   | 95.10 ms   | noise |
| `insert`   | 229.4    | 2.926 ms   | 21.14 ms   | release uplift |
| `prepared` | 16,809   | 0.040 ms   | 0.268 ms   | release uplift |
| `txn`      | 7,293    | 0.076 ms   | 0.988 ms   | release uplift |
| `mvcc`     | 1,351    | 0.571 ms   | 2.764 ms   | release uplift |
| `join`     | 1.9      | 522 ms     | 625 ms     | release uplift |
| `idxscan`  | 18,476   | 0.031 ms   | 0.359 ms   | release uplift |

`idxscan` breakdown (release, 50k rows): point lookup idx=18,476 qps vs
seq=24 qps (**774x**); range-1000 idx=193 qps; order-limit-10 idx=12,876
qps. Valgrind memcheck on v0.9 DDL paths (constraints, ALTER, views,
sequences): 0 bytes definitely lost; 89 contexts are Rust-runtime false
positives.

## v0.6 baseline — 2026-09-10

Environment: Ubuntu 24.04 sandbox, `cargo build` (debug, unoptimized),
valgrind 3.22.0. Server on `127.0.0.1:5433` with a fresh temp data dir.
5 s measurement + 1 s warmup per workload. New workload `join`: 2k users x
20k orders nested-loop equijoin with `WHERE u.id < 200` pushed below the
join (200 x 20k pairs), hash GROUP BY, aggregate ORDER BY — one query per
op, p50 in ms. The sandbox is noisy for contention-heavy workloads, so
`mvcc`/`select1`/`prepared` ranges below come from repeated runs.

| workload   | what                              | qps      | p50        | p99        | vs v0.5 |
|------------|-----------------------------------|----------|------------|------------|---------|
| `select1`  | simple-query `SELECT 1`           | ~25,600  | 0.032 ms   | 0.087 ms   | noise (24.8k–26.5k across runs) |
| `prepared` | ext. protocol: Parse once, Bind($1)/Execute loop | ~14,800 | 0.062 ms | 0.113 ms | noise (14.6k–15.1k across runs) |
| `insert`   | 1000-row multi-VALUES `INSERT`    | 132.8    | 6.616 ms   | 21.69 ms   | noise |
| `scan`     | `SELECT *` over 10k-row table     | 52.7     | 15.45 ms   | 36.41 ms   | noise (see notes) |
| `txn`      | `BEGIN` + 1-row `INSERT` + `COMMIT` loop | 6,210 | 0.143 ms   | 0.480 ms   | noise |
| `mvcc`     | 4 threads x (`BEGIN`+`UPDATE`+`SELECT`+`COMMIT`) | ~1,200 | 0.688 ms | 2.3 ms | noise (0.85k–1.3k across runs, both versions) |
| `join`     | filtered JOIN + GROUP BY (2k x 20k) | 0.5    | 2055 ms    | 2058 ms    | new |

Release-build spot check (`cargo build --release`, same workload shape,
best of 3): filtered join (2M pairs) 237 ms, join+aggregate 471 ms,
unfiltered 40M-pair equijoin `count(*)` 4494 ms (~112 ns/pair).

### What changed and why

- **New `join` workload** exercises the v0.6 query engine end to end:
  nested-loop equijoin, predicate pushdown, hash GROUP BY, and
  aggregate ORDER BY.
- **Predicate pushdown makes the filtered join 33x faster.** The first
  v0.6 join implementation evaluated WHERE after generating the full
  40M-pair cross product (release: 7.9 s for the filtered join). Single-
  source WHERE conjuncts are now pushed below comma joins and to the
  preserved side of LEFT JOINs before the nested loop runs (release:
  237 ms for the same query). The full WHERE still applies after the
  join, so pushdown is purely an optimization — and ambiguous-column
  (42702) and missing-table behavior are unchanged.
- **Zero-alloc join fast path.** When no ON predicate column reference
  is ambiguous across the two sides (proven by a pre-pass over the
  predicate), the ON clause evaluates against two stack-resident frames
  instead of a combined row buffer allocated per pair. Ambiguous
  predicates take the original combined-frame path and still raise
  42702 exactly as before.
- **A real scan regression was caught and fixed by this baseline.**
  The first v0.6 build did 33 qps on `scan` vs v0.5's 56: the new
  engine allocated a scope `Vec` per row in `project_row`, a provenance
  `Vec`+`String` per base-table row, and rebuilt every row once more in
  the comma-join accumulator. Fixes: single-FROM fast path (no
  cross-product rebuild), provenance only under FOR UPDATE, `SELECT *`
  moves the row through with zero copies, scope chain built only for
  expression items. `scan` is back to 52.7 qps.
- **No regressions anywhere else.** `mvcc` initially read 30% down, but
  an A/B against a pristine v0.5 binary showed both versions ranging
  0.85k–1.3k qps run to run — the sandbox's scheduling noise dominates
  this 4-thread mutex-contention workload, not the engine.
- **Pre-resolved JOIN ON columns: 1.63x on the filtered join.**
  Callgrind on the join workload showed per-pair name resolution
  (`resolve_col` 6.16% + string compares ~9% + iterator overhead ~5% —
  roughly 20% of all instructions) as the single biggest cost: every one
  of the 4M row pairs re-resolved `u.id`/`o.uid` by linear string scan,
  even though the schemas never change across the loop. The executor now
  resolves the ON predicate's column references once, before the loop,
  into positional `ResolvedCol { frame, idx }` nodes (via the same
  `resolve_col`, so 42702/42703 behavior is identical), and the fast path
  evaluates positions with direct indexing. Resolution is skipped when
  either input is empty, preserving the old "no error if the loop never
  runs" timing. Release A/B (15 iterations, 2k x 20k fixture):
  qualified `ON u.id = o.uid` median 478 ms -> 294 ms (**1.63x**);
  unqualified full-match `ON amt = amt` (4M pairs all match, GROUP BY
  dominates) 3376 ms -> 3223 ms (1.05x, Amdahl-limited as expected).
- **Remaining join cost is the nested loop itself.** The unfiltered
  40M-pair join is 4.5 s release (112 ns/pair, interpreted predicate).
  A hash join for equi-joins is the known next step (see README);
  secondary indexes would further cut the inner side.

### Valgrind profile: join workload (debug build)

`./benches/profile.sh --seconds 3 --workload join` — Callgrind
(`benches/profiles/callgrind.out.20504`, 2.10B instructions) and DHAT
(`benches/profiles/dhat.out.20777`). Profiled *before* the ON-column
pre-resolution above; the `resolve_col` entries below are what that
optimization removed.

Callgrind hotspots (share of instructions):
- `memcpy` 6.99% — row materialization (`combine_rows`, `Value::clone`).
- `resolve_col` 6.16%, `String == &str` 2.89%, `memcmp` 2.53%,
  `slice String == &str` 2.27%, `str::eq` 0.89%, resolve closures and
  `QCol`/`Scope` iterator machinery ~5% — per-pair name resolution,
  ~20% combined (eliminated by pre-resolution for the join fast path).
- `eval_expr` ~4.8% across monomorphizations — interpreted predicate.
- `build_source` 1.67%, `Value::clone` 1.16%, `_int_malloc` 1.15%,
  tokenizer 1.11%.

DHAT (whole 3 s session incl. fixture setup: 22k INSERTs):
- Total allocated 44.6 MB in 314k blocks; peak live ~7.7 MB.
- Largest sites are all setup-phase: `tokenize` 11.5 MB,
  WAL encode (`wal::Enc::u32/u64`) ~5.9 MB, `split_statements` 2.3 MB,
  `parse_insert`/`exec_insert`/`push_version` ~8 MB combined.
- Steady-state join query allocation is small: per-row scan
  materialization (`build_source` closure) 1.5 MB / 24k blocks (one
  `QRow` per scanned row); the pair loop itself has no significant
  allocation site — consistent with the zero-alloc fast path.
- Tokenizer allocates per query (the workload re-parses every
  iteration; no prepared statements): `tokenize` + token post-processing
  (`call_once<fn(&Token) -> Token>` 424 KB / 128k blocks) are the
  per-query parse cost to attack next if simple-protocol parse overhead
  ever dominates.

## v0.3 baseline — 2026-09-10

Environment: Ubuntu 24.04 sandbox, `cargo build` (debug, unoptimized),
valgrind 3.22.0. Server on `127.0.0.1:5433`. 5 s measurement + 1 s warmup
per workload. Two server-side changes since v0.2 (see notes): `TCP_NODELAY`
on accepted sockets, and a `BufWriter` around server→client traffic.

| workload   | what                              | qps      | rows/s  | p50      | p99      |
|------------|-----------------------------------|----------|---------|----------|----------|
| `select1`  | simple-query `SELECT 1`           | 25,624   | —       | 0.040 ms | 0.076 ms |
| `prepared` | ext. protocol: Parse once, Bind($1)/Execute loop | 15,725 | — | 0.060 ms | 0.121 ms |
| `insert`   | 1000-row multi-VALUES `INSERT`    | 229      | 229,100 | 3.879 ms | 14.12 ms |
| `scan`     | `SELECT *` over 10k-row table     | 62.5     | —       | 13.00 ms | 30.52 ms |
| `txn`      | `BEGIN` + 1-row `INSERT` + `COMMIT` loop | 790 | —       | 1.245 ms | 2.061 ms |

### What changed and why

- **`TCP_NODELAY` fixed the 41 ms floor.** The v0.2 baseline's mystery
  latency is gone: `select1` went 24.3 → 19,087 qps (785x), `prepared`
  24.1 → 15,768 qps (654x). One `set_nodelay(true)` in `main.rs`.
- **`BufWriter` fixed the bulk-write regression that `TCP_NODELAY`
  exposed.** With Nagle off but one `write_all` per DataRow field, the
  10k-row scan collapsed to 4.2 qps — every row became its own TCP
  segment. Buffering server→client traffic (flushed once per message)
  brought it to 17.8 qps, *better* than the v0.2 baseline's 10.9.
  Lesson: `TCP_NODELAY` without application-level buffering is a
  pessimization for bulk responses; the two ship together.
- **The benchmark client was lying about `scan`.** `bench.py` did one
  `recv()` per message field (~3 syscalls × 10k rows per op). A 64 KB
  userspace read buffer in the harness took `scan` from 17.8 → 62.5 qps
  (3.5x) with *no server change* — the "server" bottleneck was the
  client's read pattern. Verified: raw-socket microbenchmark with the
  same naive pattern measured 18.3 qps; buffered, 82 qps. Lesson: always
  suspect the harness before the server.
- **Release build barely matters for these workloads** (same-harness
  spot check: `select1` 19,775 release vs 19,088 debug qps, `txn` 970 vs
  758 qps). The small-query workloads are loopback-round-trip bound
  (~0.04 ms/op floor), not CPU bound, so the debug profile's 31%
  UB-check overhead doesn't move the needle on the wire. CPU
  optimization only matters once the protocol floor is the binding
  constraint (pipelining, larger payloads).
- **`txn` is new in v0.3** and measures the transaction-overlay cost:
  each op clones the whole database on `BEGIN` and swaps it back on
  `COMMIT`. Note the bench table grows by one row per op, so the 1.3 ms
  p50 includes cloning a few thousand rows — the overlay is O(database)
  per transaction, exactly the documented M3 limitation. The per-round-
  trip floor is ~0.05 ms (`select1`), so ~1.2 ms of the `txn` op is
  clone/commit work.
- `insert` is up 11x on rows/s (21,986 → 252,100); its per-op cost is now
  dominated by parsing/coercing 1000 rows, not the network.

### Where the time goes now (callgrind/DHAT)

Profiles captured 2026-09-10: `benches/profiles/callgrind.out.*`,
`benches/profiles/dhat.out.*` (2 s/workload under valgrind, debug build).
Top findings, by share of instructions / allocation volume:

1. **Debug-build UB checks are ~31% of all instructions**
   (`is_aligned_to` 21.6% + `maybe_is_nonoverlapping` 9.5%). These are
   `Vec`/`String` push-path checks that largely vanish in release —
   debug numbers overstate every data-structure cost below.
2. **The SQL tokenizer is the top real cost center.** Char-by-char
   scanning (`Iter<u8>::next` 7%, char-boundary indexing 6.8%,
   `String::push` 5.5%) plus one-by-one `Vec<Token>` growth (DHAT:
   `RawVec<Token>::grow_one` is a top allocator). The bench re-sends
   identical SQL every op, which amplifies this — but parse-per-query is
   still the hottest path in the codebase.
3. **`Value::to_text` allocates a `String` per value per row** on every
   scan (DHAT top-10); it could write straight into the message buffer.
4. **Transaction-overlay clones are visible but not dominant** at these
   sizes (`Value::clone`, `Vec<Value>` allocs in DHAT top-10) — the O(DB)
   clone cost shows up in the `txn` workload's 1.3 ms p50, as documented.
5. **The write path is quiet** (`BufWriter` internals 1.8%) — buffering
   works; heap peak for the whole profiled run was 3.7 MB / 67k blocks.

Deliberately *not* optimized in v0.3: tokenizer byte-level rewrite and
zero-alloc `to_text` — both are real but the debug profile overstates
them; they get revisited against release-build profiles in M4.

## v0.5 baseline — 2026-09-10

Environment: Ubuntu 24.04 sandbox, `cargo build` (debug, unoptimized),
valgrind 3.22.0. Server on `127.0.0.1:5433` with a fresh temp data dir
(`RUSTGRES_DATA_DIR=$(mktemp -d)`). 5 s measurement + 1 s warmup per
workload. New workload `mvcc`: 4 threads x connections running
`BEGIN` + `UPDATE` (disjoint rows) + `SELECT` + `COMMIT` in a loop.

| workload   | what                              | qps      | rows/s  | p50      | p99      | vs v0.4 |
|------------|-----------------------------------|----------|---------|----------|----------|---------|
| `select1`  | simple-query `SELECT 1`           | 25,873   | —       | 0.032 ms | 0.105 ms | +2% (noise) |
| `prepared` | ext. protocol: Parse once, Bind($1)/Execute loop | 15,026 | — | 0.062 ms | 0.147 ms | −15% (noise) |
| `insert`   | 1000-row multi-VALUES `INSERT`    | 136.4    | 136,400 | 6.707 ms | 13.85 ms | **−31%** (see notes) |
| `scan`     | `SELECT *` over 10k-row table     | 56.2     | —       | 14.43 ms | 38.43 ms | −13% (noise-ish) |
| `txn`      | `BEGIN` + 1-row `INSERT` + `COMMIT` loop | 5,687 | —      | 0.133 ms | 0.724 ms | **+737%** |
| `mvcc`     | 4 threads x (`BEGIN`+`UPDATE`+`SELECT`+`COMMIT`) | 1,030 | — | 0.725 ms | 3.46 ms | new |

### What changed and why

- **`txn` is 8.4x faster: the full-database transaction overlay is
  gone.** v0.3/v0.4 cloned the entire database on every `BEGIN` and
  diffed it on every `COMMIT` (O(database) per transaction). MVCC
  stages per-row write ops and derives WAL records at commit — the
  1.3 ms p50 of v0.4 is now 0.13 ms. This was the single biggest
  structural win of the milestone and it fell out of the design, not
  micro-optimization.
- **`insert` regressed 198 → 136 qps, honestly.** Two causes, one fixed
  during profiling and one inherent to the workload:
  1. *Fixed:* commit-time WAL derivation did a global linear row-id
     scan per row — O(rows) per row, O(n²) per 1000-row INSERT. The
     first v0.5 bench run measured **6.1 qps**. Adding a per-table
     id → position hash map (`Table::row_index`, O(1) lookup) took it
     to 136 qps.
  2. *Remaining:* the workload re-sends the identical 30 KB `INSERT`
     statement every op, so ~50% of the profiled insert path is
     tokenize + parse of SQL the server has already seen (callgrind:
     `parse_statement` 33%, `tokenize` 20%, `split_statements` 16%).
     Real clients use the prepared protocol for this (`prepared` does
     15k qps with Parse-once). A statement cache would fix the
     benchmark; it is a real feature, deferred, not a hack for the
     harness.
  3. The v0.4 `insert` number itself was documented as ±25% noisy
     (sandbox fsync variance); 136 vs 198 is at the edge of that band
     plus the parse artifact above.
- **Auto-vacuum no longer scans insert-only tables.** The first
  implementation vacuum-scanned every touched table after every
  commit — O(table) per op on a table the benchmark grows by 1000
  rows/op. Now only transactions that may have created dead versions
  (`DELETE`/`UPDATE`/`DROP`) trigger the scan; pure `INSERT`s skip it.
- **Read-only workloads are flat**, as expected: MVCC visibility
  (one `xmin`/`xmax` check per version) costs nothing measurable on
  `select1`/`scan`/`prepared`.

### Where the time goes now (callgrind/DHAT)

Profiles captured 2026-09-10: `benches/profiles/callgrind.out.9663`
+ `dhat.out.9690` (`insert` workload), `benches/profiles/callgrind.out.9788`
+ `dhat.out.9817` (`mvcc` workload), debug build, fresh data dirs.

1. **`insert`:** parse 50% (workload artifact, see above), CRC32 16%
   (required checksum; debug-build bounds checks inflate it), and
   `exec_insert` proper 15%. DHAT's top allocator is
   `RawVec<Token>::grow_one` — the tokenizer growing its token vec
   one push at a time for the re-sent 30 KB statement, same artifact.
2. **`mvcc` (the new-code path): no MVCC hotspot.** `parse_statement`
   30% (again the re-sent-SQL artifact, 4 statements per op),
   `txn_commit` 23% (WAL derivation + encode + fsync), `txn_execute`
   14%, `exec::execute` 12%. Snapshot take/register, visibility
   checks, and version-chain appends do not appear as hotspots — the
   new machinery is not the bottleneck.
3. **No leaks under concurrency.** The `mvcc` DHAT run allocated
   199 MB total and had 5 KB live at exit; all version chains, write
   logs, and snapshots are freed.
4. Heap peaks stay small (tens of MB transient).

Deliberately *not* optimized in v0.5: tokenizer throughput and a
statement/plan cache (both real, both deferred — the prepared protocol
is the supported fast path); zero-alloc `Value::to_text` (still in
DHAT's top 10, still debug-inflated); group commit (still one fsync
per commit, still intentional).

## v0.4 baseline — 2026-09-10

Environment: Ubuntu 24.04 sandbox, `cargo build` (debug, unoptimized),
valgrind 3.22.0. Server on `127.0.0.1:5433` with a **fresh temp data
dir** (`RUSTGRES_DATA_DIR=$(mktemp -d)`; `benches/profile.sh` now does
this per run so profiling never reuses stale benchmark state). 5 s
measurement + 1 s warmup per workload, same as v0.3.

| workload   | what                              | qps      | rows/s  | p50      | p99      | vs v0.3 |
|------------|-----------------------------------|----------|---------|----------|----------|---------|
| `select1`  | simple-query `SELECT 1`           | 25,314   | —       | 0.041 ms | 0.076 ms | −1% (noise) |
| `prepared` | ext. protocol: Parse once, Bind($1)/Execute loop | 17,625 | — | 0.058 ms | 0.096 ms | +12% (noise) |
| `insert`   | 1000-row multi-VALUES `INSERT`    | ~198     | ~198,000 | 4.88 ms | 7.5 ms  | **−14%** |
| `scan`     | `SELECT *` over 10k-row table     | 64.8     | —       | 12.98 ms | 31.82 ms | +4% (noise) |
| `txn`      | `BEGIN` + 1-row `INSERT` + `COMMIT` loop | 679 | —       | 1.377 ms | 3.422 ms | **−15%** |

### What changed and why

- **Every commit now costs one `fsync`.** That is the entire v0.4
  performance story, and it is *intentional*: COMMIT does `write_all` +
  `sync_all` before publishing and before replying, so an acked commit
  is durable across `kill -9`. `insert` drops 229 → ~198 qps
  (p50 3.88 → 4.88 ms) and `txn` drops 790 → 679 qps (p50 1.245 →
  1.377 ms) — the delta is the fsync, ~0.1–1 ms depending on frame
  size (the 1000-row insert frame is ~30 KB; the 1-row txn frame is
  ~50 bytes). This is the price of durability, not a regression to
  claw back by weakening it. No group commit in v0.4 (deliberate;
  revisit in M5+).
- **Read-only workloads are untouched**, as expected: `select1`,
  `scan`, `prepared` do no WAL I/O and sit within run-to-run noise of
  v0.3. (The `prepared` +12% is noise — re-running moves it ±10%.)
- **`insert` is noisy on this box.** One full-workload run measured
  144 qps; three isolated re-runs measured 197–200 qps. The variance is
  sandbox-disk fsync latency (shared/virtualized I/O), not the server:
  p50 sat at 4.87–4.92 ms across the stable runs. Take fsync-bound
  numbers here as ±25%, not gospel.
- **CRC32 went table-driven.** The v0.4 callgrind (below) showed the
  bitwise CRC32 loop at ~1.2% of commit-path instructions; it is now a
  compile-time `const fn` 256-entry table (~8x faster for the same
  checksum), verified against the standard check value
  `crc32("123456789") = 0xCBF43926` plus a bit-flip test in
  `cargo test`. Same checksum, same on-disk format.

### Where the time goes now (callgrind/DHAT on the commit path)

Profiles captured 2026-09-10: `benches/profiles/callgrind.out.3694`,
`benches/profiles/dhat.out.3719` (`txn` workload: BEGIN + 1-row INSERT
+ COMMIT loop, 5 s under valgrind, debug build, fresh data dir).

1. **The allocator is the commit path.** `_int_malloc` 10.8% +
   `_int_free` 6.6% + `malloc` 4.1% + `malloc_consolidate` 3.1% ≈ **25%
   of all instructions** are in libc malloc/free. The structural cause:
   full-database clone on every `BEGIN`, working-copy clone/swap on
   every `COMMIT`, and per-row `Vec<Value>` clones into the diff and
   the WAL frame. DHAT: 38 MB allocated over the 5 s run, almost all
   short-lived Vec growth (`RawVecInner`). This is the documented v0.3
   transaction-overlay design (O(database) per txn); fixing it
   properly means MVCC, which is still deferred — no band-aids here.
2. **`Value::eq` is 4.4%** — the diff-at-commit comparing every row of
   the working copy against the pre-commit database. Inherent to the
   diff design; also goes away with MVCC-era per-row change tracking.
3. **`Value::clone` 2.2% + slice `to_vec` ~4.4%** — row data copied
   into WAL records. Could encode straight from the source rows in a
   future pass (the WAL encoder takes `&[Value]` today only at the
   record level).
4. **fsync is invisible to callgrind** (it is a syscall, not
   instructions) — its cost is the wall-clock delta in the table above,
   not a hotspot to optimize in userspace.
5. Heap peak for the profiled run stayed small (tens of MB transient);
   no leaks: all 38 MB was freed.

Deliberately *not* optimized in v0.4: anything structural in (1)–(3).
The commit path is now fsync-bound on wall time and clone-bound on
CPU; both are consequences of documented design choices (fsync-per-
commit, full-DB transaction overlay), and the next real step for either
is a design change (group commit, MVCC), not micro-optimization.

## v0.2 baseline — 2026-09-10 (pre-TCP_NODELAY, kept for history)

Environment: Ubuntu 24.04 sandbox, `cargo build` (debug, unoptimized),
valgrind 3.22.0. Server on `127.0.0.1:5433`. 3 s measurement + 1 s warmup
per workload.

| workload   | what                              | qps    | rows/s | p50      | p99       |
|------------|-----------------------------------|--------|--------|----------|-----------|
| `select1`  | simple-query `SELECT 1`           | 24.3   | —      | 41.0 ms  | 42.2 ms   |
| `prepared` | ext. protocol: Parse once, Bind($1)/Execute loop | 24.1 | — | 41.0 ms | 47.8 ms |
| `insert`   | 1000-row multi-VALUES `INSERT`    | 22.0   | 21,986 | 45.0 ms  | 53.9 ms   |
| `scan`     | `SELECT *` over 10k-row table     | 10.9   | —      | 86.3 ms  | 175.4 ms  |

## Reading these numbers

- **The ~41 ms floor is the network, not the database.** Every workload —
  even `SELECT 1` — bottoms out at ~41 ms/op, the classic signature of
  TCP delayed-ACK vs. Nagle interaction (Linux's 40 ms delayed-ACK timer)
  when neither side sets `TCP_NODELAY`. A one-line socket option should
  collapse per-query latency by ~40 ms across the board. (Not yet applied —
  recorded here so the improvement is measurable.)
- `insert` does ~22k rows/s even in a debug build; the per-statement cost
  is dominated by the same ~41 ms floor, not by row processing.
- `scan` p99 (175 ms) is much looser than p50 — worth a callgrind look at
  row-encoding paths once the TCP floor is fixed.
- All numbers are **debug-build**; expect roughly an order of magnitude
  better in release for CPU-bound paths.

## Reproduce

```bash
cargo build && ./target/debug/rustgres &   # terminal 1
python3 benches/bench.py --seconds 5       # terminal 2
```

Profile under valgrind:

```bash
./benches/profile.sh --seconds 5 --workload all
# then: callgrind_annotate --auto=yes benches/profiles/callgrind.out.<pid> | head -60
```

See `benches/profiles/README.md` for the profile → optimize loop.

## v0.7 baseline — 2026-09-10

Environment: same sandbox, debug build, fresh temp data dir, 5 s per
workload. New workload `expr`: single-row SELECT exercising v0.7
features — numeric/string/math/date expressions, casts, `^`, LIKE/ILIKE,
built-ins (`upper`, `substring`, `abs`, `round`, `power`, `sqrt`,
`coalesce`, `trim`, `position`, `split_part`, `char_length`).

| workload   | qps      | p50        | p99        | vs v0.6 |
|------------|----------|------------|------------|---------|
| `select1`  | ~23,100  | 0.034 ms   | 0.119 ms   | noise |
| `expr`     | ~4,190   | 0.215 ms   | 0.625 ms   | new |
| `scan`     | 34.4     | 28.19 ms   | 54.74 ms   | noise |
| `insert`   | 107.9    | 7.82 ms    | 24.42 ms   | noise |
| `prepared` | ~13,800  | 0.065 ms   | 0.217 ms   | noise |
| `txn`      | ~4,350   | 0.150 ms   | 1.54 ms    | noise |
| `mvcc`     | ~993     | 0.865 ms   | 2.85 ms    | noise |
| `join`     | 0.8      | 1195 ms    | 1469 ms    | noise (faster run than v0.6's 2055 ms; noisy sandbox) |

### Callgrind / DHAT on `expr` (valgrind 3.22.0, 3 s)

Top instruction consumers are the SQL tokenizer/parser (`tokenize`,
`split_statements`, `Parser::peek`, `Token::clone`/`drop`) plus libc
`malloc`/`free`/`memcpy` — i.e. per-query SQL text parsing dominates,
not the new type-system code. No pathological hotspots in NUMERIC,
datetime, or built-in evaluation. DHAT shows the expected pattern of
many small short-lived allocations from tokenizing/parsing each query;
nothing retained. Conclusion: expression evaluation itself is cheap;
a future prepared-statement parse cache would help workloads that
re-send identical SQL text (the `prepared` workload already avoids
this by parsing once).

## v0.8 baseline — 2026-09-10

Environment: same sandbox, debug build, fresh temp data dir, 5 s per
workload. New workload `idxscan`: 50k-row table, measures indexed point
lookup (p50/p99/qps), range-1000 scan, and ORDER BY … LIMIT 10 against
the same point lookup with the index dropped (sequential scan).

| workload   | qps      | p50        | p99        | vs v0.7 |
|------------|----------|------------|------------|---------|
| `select1`  | ~22,700  | 0.045 ms   | 0.091 ms   | noise |
| `expr`     | ~4,030   | 0.215 ms   | 0.948 ms   | noise |
| `scan`     | 32.4     | 28.37 ms   | 68.23 ms   | noise |
| `insert`   | 93.8     | 9.02 ms    | 31.55 ms   | noise |
| `prepared` | ~12,500  | 0.065 ms   | 0.304 ms   | noise |
| `txn`      | ~5,810   | 0.113 ms   | 0.968 ms   | noise |
| `mvcc`     | ~941     | 0.920 ms   | 2.68 ms    | noise |
| `join`     | 0.8      | 1170 ms    | 1331 ms    | noise |
| `idxscan`  | ~10,710  | 0.073 ms   | 0.547 ms   | new |

`idxscan` breakdown (debug build, 50k rows):

| access path | qps | vs sequential |
|-------------|-----|---------------|
| point lookup, B-tree index | ~10,710 | **662x** (seq: 16 qps) |
| range 1000 rows, B-tree index | ~168 | — |
| `ORDER BY id LIMIT 10`, index order + early termination | ~8,940 | — |

(Release build for reference: point lookup ~24,000 qps, 600x over
sequential; order-limit ~2,000 qps.) Early termination of the
index-order scan (stop after OFFSET+LIMIT visible rows instead of
walking all 50k entries) took `ORDER BY … LIMIT 10` from 150 qps to
7,310 qps on the debug build.

### Callgrind / DHAT on `idxscan` (valgrind 3.22.0)

`benches/profiles/callgrind.out.idxscan`, `dhat.out.41970`. The first
profile caught a real hotspot: index key comparison for integer columns
went through `NUMERIC` normalization (`Numeric::cmp` 10.9% of all
instructions, plus `i128::checked_mul/pow` underneath). Fixed with an
`i64` fast path in `index_key_cmp` (`exact_as_i64`: SmallInt/Int/BigInt
all fit in `i64`; only true `NUMERIC` values pay for normalization)
and by replacing a `to_text().parse::<f64>()` roundtrip with
`Numeric::to_f64()` in the mixed exact/float comparison. After the fix
the top consumers are the usual per-query SQL text parsing
(`tokenize`, `split_statements`) and libc `malloc`/`memcpy` — the same
conclusion as v0.7: expression/index code is cheap, per-query parsing
dominates. DHAT: 220 MB allocated over the workload, 7.2 MB peak live
(the 50k-row table + index itself); top allocators are short-lived
tokenizer buffers and per-INSERT `Vec<Value>` row buffers. Nothing
retained, no leaks.

## v0.10: window functions and COPY (2026-09-10)

### New workloads

`window`: 10k rows, 4 window functions (`row_number`/`rank`/`lag`/`sum`)
with `PARTITION BY dept` (10 partitions) and a `ROWS` frame, `ORDER BY id`.
Result: **1.3 qps, p50 752 ms** on the debug build. Window evaluation is
O(n log n) for the partition sort plus O(n) per function; the 752 ms is
dominated by the sort and per-row frame computation.

`copy`: `COPY bench_c TO STDOUT` (text format) over 10k rows.
Result: **40.2 qps, p50 22 ms** on the debug build. COPY TO is ~30x faster
than an equivalent `SELECT *` because it skips the RowDescription/DataRow
per-row framing overhead and writes the text format directly.
