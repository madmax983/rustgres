# rustgres performance baseline

Measured with `benches/bench.py` (raw-socket wire-protocol driver, stdlib only).

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
