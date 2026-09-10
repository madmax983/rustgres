# rustgres performance baseline

Measured with `benches/bench.py` (raw-socket wire-protocol driver, stdlib only).

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
