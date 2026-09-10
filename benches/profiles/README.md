# Reading rustgres profiles

Profiles are produced by `benches/profile.sh`, which runs the server under
valgrind while `benches/bench.py` drives a fixed workload. Files:

- `callgrind.out.<pid>` — instruction-level profile: which functions burn
  CPU, call counts, cache/branch-miss simulation.
- `dhat.out.<pid>` — dynamic heap profile: total bytes allocated, peak
  live heap, allocation lifetimes.

## callgrind

```bash
# top functions by instruction count
callgrind_annotate --auto=yes callgrind.out.<pid> | head -60

# annotated source for the hottest function
callgrind_annotate --auto=yes --show-percs=yes callgrind.out.<pid> \
  | grep -A5 'fn hot_function'
```

`--collect-jumps=yes --cache-sim=yes --branch-sim=yes` were enabled, so the
annotate output also has `Ir` (instructions), `I1mr`/`ILmr` (L1/LL
instruction-cache misses), `D1mr`/`DLmr`/`Dr`/`Dw` (data cache), and `Bc`/`Bcm`
(branch / mispredict) columns. Sort by `Ir` for CPU time, by `DLmr` for
memory-stall suspects.

Tip: profile a release build for realistic numbers (`cargo build --release`
and point `profile.sh` at `target/release/rustgres`); debug builds are
fine for finding algorithmic hotspots but inflate absolute costs ~10x.

## dhat

```bash
# dh_view.html ships with valgrind at /usr/libexec/valgrind/dh_view.html:
# open it in a browser and load dhat.out.<pid> into it
```

Look at: **total bytes allocated** (allocation churn — the enemy of a fast
DB is per-query heap allocation), **peak live heap**, and the **max-lifetime**
blocks. The per-allocation-site table shows exactly which Rust code paths
allocate; cross-reference the hottest sites with the callgrind profile.

## The profile → optimize loop

1. `bench.py` gives qps/p50/p99 for each workload (the scoreboard).
2. `callgrind` says *where* time goes; `dhat` says *where* memory churn is.
3. Change server code, rebuild, re-run `bench.py` — the numbers must move.
4. Re-profile only when the scoreboard says the bottleneck moved.

Record each round in `benches/BASELINE.md` so regressions are visible.
