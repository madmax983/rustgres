# PostgreSQL 19 regression suite (vendored)

This directory holds PostgreSQL's own regression tests
(`src/test/regress` on `REL_19_STABLE`), pinned at the commit in
[`UPSTREAM`](UPSTREAM), for every suite listed in `parallel_schedule`:

| Path | Contents |
|---|---|
| `sql/<suite>.sql` | Test input, exactly as upstream |
| `expected/<suite>.out` | Primary expected output. Platform/locale alternates (`<suite>_1.out`, …) are not vendored. |
| `data/*.data` | Files that `test_setup.sql` and other suites load with `COPY` |
| `parallel_schedule` | Upstream run order |

Do not edit these files by hand. To re-vendor or bump the pin, run:

```sh
tests/conformance/vendor_pg19.sh              # re-vendor the pinned commit
tests/conformance/vendor_pg19.sh REL_19_STABLE  # bump to the branch tip
```

## Running

```sh
cargo build --release
python3 tests/conformance/schedule_runner.py --release \
    --report pg19.md --json pg19.json
```

`schedule_runner.py` runs the schedule the way `pg_regress` does:

- one database for the whole run;
- PG's real `test_setup.sql` runs first;
- then every suite runs in schedule order, each in its own session.

Objects created by earlier suites (`create_table`, `create_index`, …) are
therefore visible to later ones, exactly as upstream intends. A failed
`CREATE` cascades into later failures. The runner keeps those as
REAL-FAIL and tags them `cascade:`, so a report can tell root causes from
knock-on failures.

`schedule_runner.py` differs from the curated 22-suite gate
(`regress_runner.py` + `data/`) in two ways:

- **No legacy masks.** Every mismatch is REAL-FAIL. The one exception is
  C-language functions loaded from PG's `regress.so`, which are
  out of scope.
- **psql emulation** covers variable interpolation, `\set`, `\getenv`,
  `\if`/`\else`/`\endif`/`\quit`, and `\c`.
  - `COPY t FROM '<srcdir>/data/x.data'` is sent as `COPY ... FROM STDIN`
    with the vendored file's contents, so data loads work. Those
    statements are unscored on success.
  - Output in expanded mode (`\x`) is executed but left unscored.

Not run: `psql`, `psql_crosstab` and `psql_pipeline`. They test the
psql client itself, not the server.

## Baseline

The first full run, with a feature-gap ranking and the crash and
durability bugs it found, is in
[`docs/conformance/pg19-baseline.md`](../../../docs/conformance/pg19-baseline.md).
Re-run the schedule and update that file when a change moves the numbers.
