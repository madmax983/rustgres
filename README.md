# rustgres v0.19 — "conformance burn-down: strings type cluster"

A from-scratch PostgreSQL-compatible database server written in pure Rust —
**zero external crates**, so it builds offline with plain `cargo build`.

Milestone 19 of the road to Postgres 19 feature parity. v0.19 burns down
the strings conformance cluster: `bytea` hex I/O (with whitespace
tolerance), `reverse`/`position`/`encode`/`decode`/`crc32c` for bytea,
`sha224`/`sha256`/`sha384`/`sha512` (pure-std FIPS 180-4), a pure-std regex
engine backing `regexp_like`/`regexp_count`/`regexp_instr`/`regexp_substr`/
`regexp_replace`, `SIMILAR TO`/`NOT SIMILAR TO`, `LIKE...ESCAPE`, `strpos`,
`translate`, `unistr`, `OVERLAY`, `E'...'`/`U&'...'` literals, adjacent
literal concatenation, and POSIX `SUBSTRING(s FROM pattern)` — plus a fixed
`SUBSTRING(s FROM n FOR m)` integer regression.
Valgrind memcheck/callgrind/DHAT were not run for v0.19: Valgrind is not
installed in this environment and apt could not install it (dpkg lock held
by system processes across VM generations); this is recorded, not hand-waved.
Conformance: **2504 PASS (48.2%)**, 1556 EXPECTED-FAIL, 1133 REAL-FAIL over
5193 pg_regress statements (+119/−119 vs v0.18).
1542 cumulative protocol tests pass (27 new in v0.19), plus 73 unit tests
and 53 isolation checks.

**Repair note.** The v0.19 development pass left four defects that
independent review caught before publication: 10 of the 80 SHA-512 K
constants were mistyped (sha512 digests were wrong; sha256 was clean),
the SHA-384 IV had a typo (`85a` instead of `858`), the new regex VM hung
forever on patterns like `a(b|c)*d` (a backtracking cycle — now bounded by
a step budget that degrades to "no match" instead of hanging the backend),
and the version strings were never bumped from 0.18.0. All four are fixed
in the published tree.

## What v0.18 adds (conformance burn-down: numeric type cluster)

- **NaN/Infinity numerics.** `'NaN'::numeric`, `'inf'`, `'Infinity'` (any
  case, optional sign) parse to proper non-finite numerics. Previously an
  `INSERT` of NaN aborted the transaction and every later statement in the
  pg_regress file failed with `25P02` — a 423-error cascade from one missing
  feature. NaN/Infinity propagate through `+ - * /`, comparisons, and
  ordering with PG semantics (`-Inf < finite < Inf < NaN`; `NaN != NaN`
  while `ORDER BY` still sorts NaN last).
- **exp/ln/log at ~15 digits.** `exp`, `ln`, `log` (1- and 2-arg) compute in
  extended precision instead of erroring or returning float noise. Specials
  and SQLSTATEs match PG: `ln(0)`/`ln(negative)`/`log(0)`/`log(negative)` →
  `2201E` (numeric and float8 paths), `exp` overflow → `22003`.
- **Missing numeric built-ins.** `cbrt`, `factorial` (negative/non-integer
  → `2201F`, overflow → `22003`, capped at 1000 iterations),
  `gcd`/`lcm`, `pi()`, `degrees`/`radians` (numeric path at the engine's
  standard 10-digit division scale; float inputs stay float8),
  `scale`/`min_scale`/`trim_scale`, truncating `div` (divide-by-zero →
  `22012`), `width_bucket` (4-arg form; array-threshold form honestly
  reports `42883` as unimplemented), `random()`/`setseed()`.
- **Function column names.** `SELECT sqrt(2)`, `SELECT cbrt(8)`, etc. name
  the result column after the function instead of `?column?`.
- **Repair note.** The v0.18 development pass initially left most of these
  functions unreachable: the type resolver had no result-type entries for
  them, so every call died with `42883` before evaluation, and `pi()` /
  `random()` panicked the backend on their zero-argument call path
  (`vals[0]` on an empty slice). Independent review then caught three more
  panics the worker's tests missed: `gcd`/`lcm` called `i64::abs()` on
  their inputs, so `gcd(-2^63, 0)` killed the backend with a negation
  overflow instead of PG's `22003 "bigint out of range"` (now computed in
  `u128` with the range check); float8 `ln`/`log` returned `-Infinity`
  for `log(0)` instead of PG's `2201E`; and `min_scale('NaN')` returned
  `0` instead of PG's `NULL`. All fixed and covered by new wire-protocol
  checks in `tests/protocol_test18.py` (Groups F and G).
- **Known limitations.** `pi()` returns `numeric`, not PG's `float8`;
  extreme-scale `degrees`/`radians` inputs can hit the engine's general
  i128 `22003` overflow limit, exactly like plain `*`/`/` on the same
  magnitudes; variance/stddev aggregates and `VALUES` in derived tables
  remain unimplemented.

## What v0.17 adds (conformance burn-down: READ ONLY, datetime, COPY, version)

- **READ ONLY enforcement.** `START TRANSACTION READ ONLY`, `SET TRANSACTION
  READ ONLY` (one-shot for the next transaction), `SET SESSION CHARACTERISTICS
  AS TRANSACTION READ ONLY` (session default), and `SET
  default_transaction_read_only = on` (autocommit) all block writes with
  SQLSTATE `25006`: INSERT/UPDATE/DELETE, COPY FROM, CREATE/DROP/ALTER,
  CREATE INDEX, TRUNCATE, GRANT/REVOKE, SELECT FOR UPDATE/SHARE, and
  `nextval()`/`setval()`. Reads (SELECT, COPY TO, `currval()`) stay allowed.
  Like any statement error, a 25006 aborts the transaction (subsequent
  commands get `25P02` until ROLLBACK). Transaction control (COMMIT/ROLLBACK)
  and SET/SHOW/RESET stay usable so the mode can always be exited.
- **Date/time built-ins.** `date_part` (year/month/day/dow/hour/minute/second
  and friends; bogus field → `22023`), `to_date`/`to_timestamp`/`to_char`
  (format-subset: YYYY/MM/DD/HH24/MI/SS), `to_timestamp(float8)` (Unix epoch),
  `make_date`/`make_timestamp` (out-of-range → `22008`), `timezone('UTC', ...)`
  (non-UTC → `0A000`), `clock_timestamp()`, `statement_timestamp()`,
  `transaction_timestamp()`. `age`, justify functions, `make_interval`,
  `make_time`, `date_bin` remain `42883` (unimplemented, not wrong).
- **Extended-protocol COPY.** `COPY table TO STDOUT` and `COPY table FROM
  STDIN` now work via Parse/Bind/Describe/Execute: Describe returns NoData
  (formats ride in the Copy response), Execute drives CopyOutResponse/
  CopyData/CopyDone/CommandComplete or CopyInResponse + CopyData/CopyDone/
  CommandComplete. Errors (bad table → `42P01`, bad data → `22P02`, read-only
  → `25006`, aborted txn → `25P02`) surface as ErrorResponse with Sync
  recovery, like other extended-protocol statements.
- **Honest version reporting.** rustgres no longer claims to be PostgreSQL
  16.0. Startup `server_version` is `0.17.0`, `SHOW server_version_num`
  is `1700`, and `version()` returns `rustgres 0.17.0
  (PostgreSQL-compatible, protocol 3.0)`.
- **Known limitations.** `SET TRANSACTION` outside a transaction block
  applies to the next transaction (PG would warn); `timezone()` only
  supports UTC; `RESTART IDENTITY` still `0A000`.

## What v0.15 adds (conformance burn-down: transaction syntax)

- **Full transaction-control syntax.** `BEGIN [WORK | TRANSACTION]`;
  `START TRANSACTION` with `ISOLATION LEVEL {SERIALIZABLE |
  REPEATABLE READ | READ COMMITTED | READ UNCOMMITTED}`, `READ WRITE` /
  `READ ONLY`, `[NOT] DEFERRABLE` in any comma-separated combination;
  `COMMIT` / `END` / `ROLLBACK` / `ABORT` with optional `WORK` /
  `TRANSACTION` and `AND CHAIN` / `AND NO CHAIN`. Transactions retain
  isolation, read-only, and deferrable characteristics; `AND CHAIN`
  commits/rolls back with the correct command tag and immediately opens
  the next transaction with the same characteristics, like PostgreSQL.
  transactions regression test: 275 PASS / 37 REAL-FAIL (was 257/55).
- **Read-path profiling fix.** Valgrind memcheck on a 200-statement
  txn-heavy workload: zero invalid accesses, zero leaks. Callgrind+DHAT
  showed `read_bounded` allocating a fresh 64 KiB zeroed scratch buffer
  per inbound message — 18.5 MiB total for the workload at 283 B
  max-live, 76% of all profiled instructions. The scratch chunk is now a
  thread-local reused across messages on each connection thread
  (thread-per-connection makes this safe); behavior is unchanged and
  covered by a new `repeated_read_message_reuses_scratch_cleanly` unit
  test plus the full protocol suite.
- **Known limitations.** `READ ONLY` is parsed and retained but not
  enforced (a write inside `START TRANSACTION READ ONLY` still
  executes); the largest remaining conformance clusters are missing
  built-ins (`substr`, `concat`, `concat_ws`, `to_hex`, `sign`,
  `left`/`right`, `reverse`, ...), `FETCH` cursor syntax, and bare
  `TRUNCATE`.

## What v0.14 adds (pg_regress conformance harness)

- **Conformance runner.** `python3 tests/conformance/regress_runner.py`
  runs 22 PostgreSQL regression tests against rustgres, comparing
  actual output to PostgreSQL's expected `.out` files with a semantic
  (not textual) diff. Results are classified as PASS, EXPECTED-FAIL
  (known gap with a documented reason), or REAL-FAIL (unexpected
  divergence). Baseline: 1661 PASS (32.0%), 1644 EXPECTED-FAIL, 1888
  REAL-FAIL out of 5193 statements.
- **Isolation specs.** `tests/conformance/isolation_specs.py` translates
  PostgreSQL's `simple-write-skew.spec` and
  `insert-conflict-do-nothing.spec` plus a custom READ COMMITTED
  visibility spec: 53 checks pass. Documents deviations: no SSI
  predicate tracking (serial-equivalent outcomes observed), concurrent
  `ON CONFLICT DO NOTHING` fails the loser with `40001` instead of
  blocking, and READ COMMITTED losers can transiently see both rows.
- **Commit-time unique recheck.** `Database::committed_unique_violation`
  + `records_for_commit` re-verifies unique keys against rows committed
  after the statement snapshot. Fixes a real race: two concurrent
  transactions inserting the same primary key would both commit
  duplicates. Now the loser gets `40001`.
- **Parser/executor gaps closed for conformance.** Type aliases
  (`int4`, `varchar`, `char`, `bpchar`, `name`, `serial`), typmods
  (`varchar(10)`), `VACUUM ANALYZE`, int→bool casts, boolean
  `'tru'`/`'of'` prefixes, `booleq`/`boolne`/`int4eq`/`texteq`,
  `float8(x)` function-style casts, `1::int` in INSERT VALUES, `CREATE
  TEMP TABLE` (parses as persistent; documented), alias-less FROM
  subqueries, `(VALUES ...)` sources, auto-named `CREATE INDEX`.
- **Known limitations.** Correlated `IN` subqueries are O(n²);
  cartesian joins materialize fully (OOM risk on huge cross joins);
  TEMP tables are fake-persistent; extended-protocol COPY absent;
  `server_version` reports 16.0.

## What v0.13 adds (replication protocol, logical decoding)

- **Walsender dialect.** A startup packet with `replication=true`
  (superusers only, `42501` otherwise) routes the connection to the
  replication command loop after the normal auth/handshake. Supported
  commands: `IDENTIFY_SYSTEM` (exact `systemid/timeline/xlogpos/dbname`
  shape; the system id is generated once per data directory and stable
  across restarts), `CREATE_REPLICATION_SLOT` (logical slots with the
  `rustgres_decoding` plugin; `PHYSICAL` slots accepted as metadata),
  `DROP_REPLICATION_SLOT`, and `START_REPLICATION SLOT name LOGICAL
  lsn`. Unknown commands → `42601`; `TIMELINE_HISTORY` and
  `BASE_BACKUP` → `0A000` (single timeline; no base backup — documented
  future work). Every command error is followed by `ReadyForQuery`, so
  the client can continue.
- **Durable slots.** Slot create/drop/flush are WAL-logged
  (`ReplSlotCreate`/`ReplSlotDrop`/`ReplSlotFlush` records) and
  checkpointed (checkpoint format v6, WAL magic `RGSWAL07` — older data
  directories are refused loudly). Creating a duplicate slot → `42710`,
  bad names → `42602`, unknown plugins → `0A000`, dropping a missing
  slot → `42704`, dropping an active slot → `55006`.
- **`START_REPLICATION` streaming.** The server replies
  `CopyBothResponse`, then streams `CopyData` frames: `d` XLogData
  (data_start, wal_end, send timestamp, logical line) and `w`
  keepalives every 10 s while idle (no reply requested). The client
  answers with `r` standby-status messages; the server advances the
  slot's `confirmed_flush_lsn` (WAL-logged periodically) and
  `restart_lsn`. `CopyDone` ends the stream and returns the connection
  to command mode (`ReadyForQuery`); `Terminate` closes it. Streaming
  marks the slot `active`; every return path deactivates it.
- **Logical decoding (`rustgres_decoding`).** Each committed WAL frame
  decodes to `BEGIN <lsn>` … `COMMIT <lsn>` with one line per change:
  `INSERT t id=1 name='alice'`, `UPDATE t OLD id=2 name='bob' NEW id=2
  name='bobby'`, `DELETE t id=1 name='alice'`, `DDL CREATE_TABLE t`.
  UPDATE is first-class end to end (WAL `UpdateRows` carries old+new
  row images; DELETE carries old rows), so logical streams show exact
  before/after values. Non-table records (sequences, roles, ACLs, slot
  metadata) are invisible to the decoder.
- **`pg_replication_slots`.** A virtual catalog view over the slot map:
  `slot_name, plugin, slot_type, active, restart_lsn,
  confirmed_flush_lsn` (LSNs in `X/Y` text form), visible to normal SQL
  clients.
- **Performance.** Valgrind memcheck over a replication workload (slot
  create, DML, logical streaming, standby status, drop): 0 bytes
  definitely/indirectly lost, 0 invalid accesses. Callgrind: no
  v0.13-specific hotspots (top costs are the pre-existing table-driven
  `wal::crc32` and WAL frame decode). DHAT: 3.0 MB total, 84.6 KB peak
  live, all short-lived. Wire benchmarks (`benches/BASELINE.md`):
  CPU-bound p50s unchanged vs v0.12 (`select1` 0.026 ms, `prepared`
  0.059 ms, `idxscan` 0.036 ms); `txn` 8,693 qps / 0.073 ms. Logical
  streaming: 502 change lines with 0.00 ms steady-state inter-line gap.
  A mid-milestone `txn` scare (12–59 ms COMMITs) was bisected to the
  data dir sitting on btrfs vs `/tmp` (tmpfs) — v0.13 matches v0.12 to
  the microsecond on the same filesystem; the commit path is unchanged.
- **Compatibility.** Normal clients are unaffected: `replication` unset
  or `replication=database` behaves exactly as before (all 1051 prior
  protocol checks still pass unchanged).

Known v0.13 deviations/limitations (all documented, none silent): **no
base backup** (`BASE_BACKUP` → `0A000`); **no physical streaming**
(physical slots are metadata only); single timeline; `UPDATE` decoding
uses the table's current schema (a concurrent `ALTER TABLE` between
commit and decode can mislabel columns — positional fallback after
`DROP`); replication slots are cluster-global (Postgres scopes them per
database); `server_version` still reports `16.0`; still no group
commit and the engine is still globally mutex-serialized.

## What v0.12 adds (concurrency, soak, hardening)

- **Allocation-DoS resistance.** Frontend message lengths are validated
  *before* allocating: reads proceed in bounded 64 KiB chunks, startup
  packets are capped at 16 MiB, and regular frontend messages at 1 GiB
  (PostgreSQL's documented ceiling). A client claiming a 2 GiB message
  no longer makes the server allocate 2 GiB.
- **Protocol correctness.** Unknown frontend message types now return
  `08P01` (protocol violation) and FATAL-close the connection, as
  PostgreSQL does (previously `0A000` without closing).
  `CancelRequest` (`80877102`) is recognized and the connection closed
  quietly; `SSLRequest` is refused with `N`, then normal startup works.
- **Poisoned-lock recovery.** If a worker thread panics while holding
  the engine or WAL mutex, subsequent connections no longer die from
  mutex poisoning: the lock is recovered and a warning logged. (Mid-
  statement panic consistency is not formally guaranteed.)
- **Concurrency stress.** `tests/protocol_test12.py` runs threaded
  workloads: mixed inserters/readers/transactional updaters with a
  primary-key uniqueness invariant, same-name `CREATE`/`DROP` races,
  concurrent `GRANT`/`REVOKE` vs privilege enforcement (readers observe
  both allow and deny, never a wrong code), concurrent `nextval`
  uniqueness (160/160 unique), `CHECKPOINT`/`VACUUM` under write load,
  and 150 rapid connect/disconnect cycles. No stuck threads, no wrong
  answers. Lock-order audit: every nested site locks engine → WAL,
  never the reverse.
- **Soak + crash recovery.** A randomized 120 s soak (30k statements:
  DML, transactions, DDL, indexes, checkpoints, CTEs, aggregates) ran
  with zero unexpected SQLSTATEs and flat RSS (9→11 MB), then the
  server was `SIGKILL`ed mid-run and restarted: all soak tables
  readable after WAL recovery.
- **SQLSTATE audit.** Duplicate column definitions
  (`CREATE TABLE t(a INT, a INT)`), duplicate `INSERT` target columns,
  duplicate `UPDATE ... SET` targets, and duplicate `ON CONFLICT DO
  UPDATE SET` targets all now return `42701` (previously `42601` or
  silently accepted). New pins cover `08P01`, `34000`, `26000`,
  `42501`, `23505`/`23503`/`23502`/`23514`, `42P01`/`42P07`,
  `42703`/`42702`/`42701`, `42803`, `42883`, `22P02`, `22012`,
  `40001`, `25P02`, `25001`, `3B001`, `28P01`, and more.
- **Performance.** Valgrind memcheck over a threaded stress workload: 0
  bytes definitely/indirectly lost, 0 memory errors. Callgrind: no
  v0.12-specific hotspots (top named costs are pre-existing i128
  numeric arithmetic and allocation). DHAT: 72 MB total allocated,
  all short-lived, no heap bloat. Wire fuzz sustained ~5,000 qps
  through a 3,000-query flood.

Known v0.12 deviations/limitations (all documented, none silent):
**`CancelRequest` closes quietly without cancelling a backend**;
`COPY` inside explicit transactions uses a throwaway snapshot;
extended-protocol `COPY` is absent; `DELETE … WHERE id IN (subquery)`
is unsupported; `INSERT … SELECT` skips complete target-column
coercion; **no group commit** (WAL fsync per commit holds the engine
lock); the engine is globally mutex-serialized (correctness over
throughput for now); `server_version` still reports `16.0`.

## What v0.11 adds (SCRAM auth, roles, grants)

- **SCRAM-SHA-256.** `RUSTGRES_AUTH=scram-sha-256` enables full RFC 7677
  authentication (raw-wire SASL, no SASLprep); trust mode otherwise.
  Unknown users get a dummy verifier so they are not enumerable by
  timing; expired passwords fail with `28P01`. Authorization (database
  `CONNECT` + `CONNECTION LIMIT`) now runs *before* `AuthenticationOk`,
  like PostgreSQL, and the connection-limit slot is an RAII guard so it
  cannot leak on I/O failures.
- **Roles.** `CREATE ROLE ... [NO]LOGIN [SUPERUSER] PASSWORD '...'`
  `CONNECTION LIMIT n VALID UNTIL '...'`, `ALTER ROLE`, `DROP ROLE`
  (cleans up membership edges), transactional with WAL/checkpoint
  durability. Bootstrap `postgres` superuser.
- **Memberships.** `GRANT group TO member` / `REVOKE group FROM member`
  (superuser-only), transitive privilege inheritance (ownership and
  superuser never inherit), duplicate grants are no-ops, self-grants
  and cycles rejected (`42501`), missing roles are `42704`.
- **Privileges.** `GRANT SELECT/INSERT/UPDATE/DELETE/TRUNCATE/
  REFERENCES/TRIGGER ON table`, column lists (`GRANT SELECT (a, b) ON
  t`), `GRANT USAGE/SELECT ON SEQUENCE`, `GRANT/REVOKE CONNECT ON
  DATABASE`. Enforcement covers `SELECT` (incl. `*` and every clause
  that reads a column), `INSERT`/`UPDATE` per-column, `SELECT FOR
  UPDATE`, and `nextval`.
- **Ownership.** `ANALYZE`/`VACUUM` on a table and `CREATE OR REPLACE
  VIEW` require owner or superuser (`42501`); bare `ANALYZE`/`VACUUM`
  only touch owned tables.
- **Catalogs.** `pg_authid`, `pg_roles`, `pg_user` (passwords masked,
  `rolvaliduntil`/`valuntil` exposed), `pg_auth_members`
  (`roleid`/`member`/`grantor`/`admin_option`).
- **Performance.** Callgrind-driven: the per-query role-membership
  closure is now built once per statement instead of once per column
  (~2x fewer instructions in permission checks). Valgrind memcheck: 0
  bytes definitely lost on the auth/grant paths; DHAT: no heap bloat.

Known v0.11 deviations/limitations (all documented, none silent): **no
SASLprep** (raw UTF-8); salt/nonce entropy is time+pid+xorshift, not an
OS CSPRNG; SCRAM iterations default to 4096; **no channel binding**;
`WITH GRANT OPTION` / `WITH ADMIN OPTION` parsed but without
semantics; **no `CREATEROLE`** — role administration is superuser-only;
multiple objects in one `GRANT`/`REVOKE` rejected; views execute as the
invoking role (not view-owner); `TRUNCATE` privilege exists but the
statement is unimplemented; `REVOKE` of a non-edge is a silent no-op
(Postgres warns); `server_version` still reports `16.0`.

## What v0.10 adds (CTEs, window functions, upsert, RETURNING, COPY) v0.10 adds **CTEs**
(`WITH`, `WITH RECURSIVE`, multiple CTEs, DML CTEs), **window functions**
(`row_number`, `rank`, `dense_rank`, `lag`, `lead`, `first_value`,
`last_value`, `nth_value`, `ntile`, aggregate windows with `PARTITION BY` /
`ORDER BY` / `ROWS`/`RANGE` frames), **UPSERT** (`ON CONFLICT (columns) DO
UPDATE SET` / `DO NOTHING` with `EXCLUDED`), **RETURNING** for
INSERT/UPDATE/DELETE, and **COPY** (`COPY TO/FROM STDOUT/STDIN` with text/CSV
formats, options, and protocol messages). 868 cumulative protocol tests
pass (70 new in v0.10).

## What v0.10 adds (CTEs, windows, upsert, RETURNING, COPY)

- **CTEs.** `WITH cte AS (...) SELECT ...`, `WITH RECURSIVE` (UNION ALL
  iteration), multiple CTEs, CTEs referenced from joins/subqueries/DML.
- **Window functions.** `OVER (PARTITION BY ... ORDER BY ... ROWS/RANGE ...
  )` with ranking (`row_number`, `rank`, `dense_rank`, `ntile`), navigation
  (`lag`, `lead`, `first_value`, `last_value`, `nth_value`), and aggregates
  (`sum`, `avg`, `count`).
- **UPSERT.** `INSERT ... ON CONFLICT (cols) DO UPDATE SET col = EXCLUDED.col`
  and `DO NOTHING`, with unique-index/constraint inference.
- **RETURNING.** `INSERT/UPDATE/DELETE ... RETURNING` with row-bearing
  results in both simple and extended protocols.
- **COPY.** `COPY t TO STDOUT` / `COPY t FROM STDIN` with text and CSV
  formats, `DELIMITER`/`NULL`/`HEADER` options, and line-numbered errors
  for malformed input.
  `NOT NULL`, `CHECK (expr)`. Proper SQLSTATE codes (`23502`, `23503`,
  `23505`, `23514`). FK actions cascade correctly across tables.
- **ALTER TABLE.** `ADD COLUMN` (with `DEFAULT` backfill), `DROP COLUMN`
  (with `CASCADE`/`RESTRICT` dependency checking), `RENAME COLUMN`,
  `ADD CONSTRAINT`, `DROP CONSTRAINT`, `RENAME TO`. All transactional
  with WAL durability and MVCC-safe version swapping.
- **Views.** `CREATE [OR REPLACE] VIEW`, `DROP VIEW [CASCADE]`.
  Dependency tracking prevents dropping tables/views with dependents
  (unless `CASCADE`). Views are fully transactional and WAL-durable.
- **Sequences.** `CREATE SEQUENCE` with `START WITH`, `INCREMENT BY`,
  `MINVALUE`/`MAXVALUE`, `CYCLE`/`NO CYCLE`. `nextval()`, `currval()`,
  `setval()` with session-local `currval` state. Sequences advance
  non-transactionally (like Postgres) and survive crashes via WAL.
  `DEFAULT nextval('seq')` works for auto-increment columns.
- **Catalogs.** `information_schema.tables` and
  `information_schema.columns` for basic schema introspection.
- **Transactional DDL.** `ALTER TABLE` and `CREATE VIEW` roll back
  correctly. Sequence advancements are NOT rolled back (Postgres
  semantics).

## What v0.8 adds (B-tree indexes, planner, ANALYZE, EXPLAIN)

- **Secondary indexes.** `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON
  table(cols...)`, `DROP INDEX [IF EXISTS] name`. Composite keys with
  lexicographic ordering; NULLs sort high (Postgres default), so `ASC`
  reads NULLS LAST and `DESC` reads NULLS FIRST. Scalar ordering for all
  indexable types (ints, floats, numerics, text, bool, dates,
  timestamps, bytea, UUIDs).
- **Planner.** Equality on a leading index prefix becomes an index point
  lookup; `>`/`>=`/`<`/`<=`/`BETWEEN` on the next column (or the leading
  column) become range bounds; composite `a = ? AND b > ?` uses both.
  Compatible `ORDER BY ... LIMIT` skips the sort and streams from the
  index with early termination. Every index scan rechecks MVCC
  visibility and applies the full residual predicate afterwards, so a
  plan only ever costs speed.
- **UNIQUE.** Statement-atomic: one bad row in a multi-row `INSERT`
  rolls back the whole statement with `23505`. Unique checks see
  snapshot-visible rows (own txn's rows always count).
- **Transactional DDL.** `CREATE`/`DROP INDEX` inside a transaction are
  visible to your own snapshot and roll back cleanly; other
  transactions never see uncommitted indexes.
- **ANALYZE.** Per-column stats: row count, null fraction, exact
  distinct count, most-common values + frequencies, histogram bounds.
  Visible through the virtual `pg_stats` table (a real table named
  `pg_stats` takes precedence, like Postgres). Statistics live in
  memory and are rebuilt by `ANALYZE` after a restart; the planner is
  rule-based (longest bound prefix wins), so plans never depend on
  stats being present.
- **EXPLAIN.** `EXPLAIN SELECT ...` prints a Postgres-style plan tree
  (works in simple and extended protocol). `EXPLAIN ANALYZE` is
  rejected with `0A000` (feature not supported), like a real missing
  feature.
- **Durability.** WAL magic `RGSWAL04`, checkpoint magic `RGSCHK04`
  (version 4). Index definitions and entries survive `kill -9` and
  checkpoint restarts. **v0.7 data directories are loudly refused at
  startup** — the on-disk encoding changed.

## What v0.7 adds (richer types, casts, operators, built-ins)

- **New types.** `SMALLINT` (int2), `BIGINT` (int8), `REAL` (float4),
  `NUMERIC` (arbitrary-precision decimal), `DATE`, `TIMESTAMP`,
  `TIMESTAMPTZ` (UTC-only), `BYTEA` (hex output, hex/escape/octal input),
  `UUID`. All work with MVCC, WAL/checkpoint replay, JOINs, aggregates,
  and GROUP BY. Integer literals outside the int4 range become `BIGINT`,
  like Postgres.
- **Casts.** `expr::type` and `CAST(expr AS type)` between all pairs
  Postgres allows (text↔numeric, text↔date, date→timestamp, …).
  Bad input is `22P02`, impossible casts are `42846`.
- **Operators.** `^` for exponentiation (exact `NUMERIC` power for
  integer exponents, `float8` otherwise; unary minus binds tighter,
  like Postgres: `-2^2 = 4`). `%` works for exact numerics including
  `NUMERIC` (not for `real`/`double`, like Postgres). `LIKE`/`ILIKE`
  (with `ESCAPE`), `BETWEEN`, `||` concatenation.
- **Numeric promotion.** `smallint+smallint→int`,
  `int+bigint→bigint`, `int+real→real`, `int+numeric→numeric`,
  `real+double→double`, like Postgres. Division: integer division
  truncates, `NUMERIC` division is exact (10 guard digits, normalized),
  float division by zero is `22012` (like Postgres).
- **Built-ins.** String: `upper`, `lower`, `length`, `char_length`,
  `substring` (both forms), `trim`, `position` (both forms), `replace`,
  `split_part`. Math: `abs`, `round`, `floor`, `ceil`/`ceiling`, `sqrt`,
  `power`, `mod`. Datetime: `now`, `current_date`,
  `current_timestamp`, `date_trunc`, `extract`. Conditional: `coalesce`,
  `nullif`, `greatest`, `least` (NULL-ignoring, like Postgres).
  Wrong arity and unknown functions are `42883`, like Postgres.
- **Date arithmetic.** `date ± integer → date`, `date - date →
  integer` days. No `INTERVAL` type yet (timestamp arithmetic is
  `42883`).
- **NULL handling.** Three-valued logic throughout, `IS [NOT]
  TRUE/FALSE/UNKNOWN`, explicit `NULLS FIRST`/`LAST` (defaults match
  Postgres: `ASC` → nulls last, `DESC` → nulls first), aggregate
  `DISTINCT`, `string_agg` (NULL-ignoring, NULL delimiter = no
  separator).
- **Persistence.** WAL magic `RGSWAL03`, checkpoint magic `RGSCHK03`
  (version 3). **v0.6 data directories are loudly refused at startup**
  — the on-disk encoding changed.

Known v0.7 deviations/limitations (all documented, none silent):
**NUMERIC is limited to ~38 significant digits** (Postgres allows far
more); NUMERIC division computes 10 fractional guard digits then
normalizes (Postgres emits 20 fractional digits); NUMERIC `sqrt` and
non-integer `power` use `f64` (~15–16 digits); **TIMESTAMPTZ is
UTC-only** (no session timezone; zone-less input is UTC); leap seconds
clamp to `:59`; **no INTERVAL type**; mixed exact/float comparisons use
`f64`; `numeric(p,s)` modifiers are parsed but not enforced; `SUM`
keeps the input type instead of Postgres' int→bigint widening; `AVG`
is double precision (Postgres uses exact numeric for exact inputs);
`GREATEST`/`LEAST` ignore NULLs (Postgres returns NULL if any arg is
NULL — deliberate); `string_agg` ignores NULLs; decimal literals are
`float8` in expressions (Postgres parses them as numeric) but INSERT
preserves exact text for NUMERIC targets; `VALUES` only accepts
literals (and `$N` params), not expressions; no `CASE` expressions yet.

## What v0.6 adds (query engine)

- **JOINs.** `INNER JOIN` / `LEFT JOIN` / `CROSS JOIN` with arbitrary `ON`
  predicates (not just equijoins), comma joins, table aliases, qualified
  `t.col` references, and derived tables (`FROM (SELECT ...) AS s`).
  Ambiguous unqualified columns are `42702`, missing tables/columns are
  `42P01`/`42703` — like Postgres. Execution is nested-loop with two
  executor-level optimizations (see `benches/BASELINE.md`): single-source
  `WHERE` conjuncts are pushed below the join, and unambiguous `ON`
  predicates evaluate against two stack-resident frames with zero
  allocation per row pair.
- **Subqueries.** Scalar subqueries, `IN` (subquery and list forms), and
  `EXISTS` — all correlatable to the outer query. Derived tables are
  uncorrelated (no `LATERAL` support yet).
- **Aggregates.** `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, multi-column
  `GROUP BY`, `HAVING`, `COUNT(*)` vs `COUNT(col)` NULL semantics,
  `sum()` of no rows is NULL, and `ORDER BY` an aggregate (or any
  group-level expression) that isn't in the select list. Aggregates are
  rejected in `WHERE`/`JOIN ON`/`GROUP BY` with `42803`, like Postgres.
- **`SELECT ... FOR UPDATE`.** Row-level locks on the base-table versions
  behind the result rows (through joins too). Lock conflicts fail
  immediately with `40001` — there is **no lock waiting** (deliberate;
  see below). Locks release on commit, rollback, savepoint rollback
  (only locks taken after the savepoint), and disconnect. Writers still
  check `FOR UPDATE` locks, so a locked row can't be concurrently
  updated/deleted.
- **Extended protocol throughout.** `$N` parameters work in `JOIN ON`,
  subqueries, `HAVING`, and `ORDER BY`, with type inference and
  `Describe` support.
- **NULL semantics.** Three-valued predicate logic (`WHERE NULL` filters,
  `NULL = NULL` is not true), `IS [NOT] NULL`, `COUNT(col)` skips NULLs,
  and Postgres null placement in `ORDER BY`.

Known v0.6 deviations/limitations (all documented, none silent): **no
lock waiting** — `FOR UPDATE` on a locked row raises `40001` immediately
instead of blocking; **no `LATERAL`** — derived tables can't correlate;
**`FOR UPDATE` inside an `UPDATE ... SET` subquery is evaluated but
takes no locks** (there is no statement-level lock flow for `UPDATE`;
documented in `eval_update_expr`); **nested-loop joins only** — no hash
join yet, no secondary indexes (every join scans the inner side; the
`join` benchmark in `benches/BASELINE.md` quantifies this); **no CTEs,
no window functions, no set operations** (`UNION`/`INTERSECT`/`EXCEPT`);
**no `RIGHT`/`FULL` joins**; scalar subqueries must return exactly one
row; still a small SQL/type/function surface (no `LIKE`, no casts, text
sort is byte-wise).

## What v0.5 adds (MVCC + isolation)

- **Row versions, not row locks.** Each row version carries `xmin`
  (creating xid) and `xmax` (deleting/updating xid, 0 = live).
  `UPDATE` = stamp the old version's `xmax` + append a new version;
  `DELETE` = stamp `xmax`. Tables are versioned identically
  (`created_xmin`/`dropped_xmax`), so `CREATE`/`DROP TABLE` are
  transactional too. A version is visible to a snapshot iff its `xmin`
  committed before the snapshot and its `xmax` did not — the standard
  MVCC visibility rule, plus "own writes are always visible to self".
- **Isolation levels.** `BEGIN [ISOLATION LEVEL { READ COMMITTED |
  REPEATABLE READ | SERIALIZABLE }]` (also `START TRANSACTION ...`;
  `READ UNCOMMITTED` is accepted and treated as `READ COMMITTED`, like
  Postgres). `READ COMMITTED` takes a fresh snapshot per statement
  (sees newly committed rows, including phantoms); `REPEATABLE READ`
  and `SERIALIZABLE` pin one snapshot at the first snapshot-taking
  statement and hold it to COMMIT. Aborted transactions behave like
  Postgres: the first error poisons the transaction (`25P02` on anything
  but `COMMIT`/`ROLLBACK`), and `SAVEPOINT` / `ROLLBACK TO SAVEPOINT`
  undo MVCC writes in reverse order.
- **`UPDATE` / `DELETE` with `WHERE`.** Full predicate support shared
  with `SELECT`; `UPDATE t SET a = a + 1` works; tags are `UPDATE n` /
  `DELETE n`. Statement atomicity is preserved: the statement plans
  (validates + conflict-checks) before mutating, so a failed statement
  leaves no trace.
- **`VACUUM [VERBOSE] [table]`.** Dead versions (invisible to every
  active snapshot *and* every possible future snapshot) are physically
  reclaimed; `VERBOSE` reports per-table counts. An automatic
  best-effort vacuum runs after every commit/abort, but only on tables
  where the transaction may have created dead versions
  (`DELETE`/`UPDATE`/`DROP` — pure `INSERT`s skip the scan). Open
  snapshots pin the versions they can still see: a `REPEATABLE READ`
  transaction holding an old snapshot blocks reclamation until it ends.
  `VACUUM` is rejected inside a transaction (`25001`), like Postgres.
- **`ORDER BY` (bonus).** `ORDER BY expr [ASC|DESC] [, ...]`, positional
  `ORDER BY 1`, sorting by non-selected columns, `LIMIT` applied after
  the sort, and Postgres null placement (`NULLS LAST` for ASC,
  `NULLS FIRST` for DESC). Text sorts byte-wise — no collations yet.
- **WAL v2.** Records are now per-version rather than per-diff:
  `CreateTable`, `InsertRows` (grouped per table, carrying stable row
  ids + `xmin`), `DeleteRows` (row ids + deleter `xmax`), `DropTable`.
  The on-disk magic is `RGSWAL02` / `RGSCHK02`: **v0.5 refuses to open
  v0.4 data directories** (loud error, not silent truncation — start
  fresh or keep a v0.4 binary for old data).
- **Row-id index.** Every table keeps an id → position hash map, so
  commit-time WAL derivation and undo are O(1) per row instead of O(n)
  scans — a 1000-row `INSERT` went from 6 qps back to competitive after
  this landed (see `benches/BASELINE.md`).

### v0.5 concurrency model (read this before benchmarking)

- **One global engine mutex, held per statement.** There is no row
  locking and no waiting: a concurrent writer never blocks. Instead,
  conflicts are detected and reported as `40001`
  (`serialization_failure`):
  - `REPEATABLE READ` / `SERIALIZABLE`: writing a row that changed after
    your snapshot → `40001`, like Postgres.
  - Lost-update race (two transactions `UPDATE` the same row while both
    uncommitted): the first committer gets `40001` rather than silently
    duplicating the row; the second committer wins.
  - Concurrent `CREATE TABLE` of the same name: second committer gets
    `40001`.
- **`SERIALIZABLE` is snapshot isolation, not full SSI.** It gives you a
  stable snapshot plus the write-conflict detection above, but it does
  **not** prevent write skew: two concurrent serializable transactions
  can each pass a `SELECT`-based check and then both commit overlapping
  writes. Documented and tested (`protocol_test5.py` asserts both
  commits succeed) — predicate locking / SSI is a later milestone.
- **Disconnects roll back.** A client that disconnects — clean
  `Terminate`, raw socket close, or read error — with an open
  transaction gets its writes undone and its xid retired, via the same
  `txn_rollback` path as `ROLLBACK`. Uncommitted versions never leak
  into the shared tables and abandoned xids never pin snapshots or
  vacuum.

Known v0.5 deviations/limitations (all documented, none silent):
**no row locking / blocking** (see above — conflicts become `40001`
instead of waits, which is stricter than Postgres in a few
`READ COMMITTED` re-evaluation cases); **SERIALIZABLE ≠ SSI** (write
skew possible); **no predicate locking, no `SELECT ... FOR UPDATE`**;
**no secondary indexes** (every `WHERE` is a full version-chain scan);
**autovacuum is best-effort and lazy** (a snapshot released by a
read-only transaction can leave dead versions until the next
delete-carrying commit or an explicit `VACUUM`); **no collations**
(`ORDER BY` on text is byte-wise); **no aggregates** (`COUNT(*)` etc.
are still unimplemented); **WAL v2 is incompatible with v0.4 data
directories**.

## What v0.4 adds (durability)

- **Write-ahead log** (`src/wal.rs`, pure std, zero crates). The storage
  engine has no pages — it is a `HashMap<String, Table>` — so the WAL is
  *logical*, at the granularity the engine actually mutates:
  - `CreateTable { name, columns }`, `InsertRows { table, rows }`,
    `DropTable { name }`
  - `FullTable { name, columns, rows }` — fallback when a commit's delta
    is not a pure append (e.g. a transaction whose working copy predates
    another session's commit, so the diff degrades to a whole-table
    image; replay overwrites exactly as the in-memory swap did).
- **Commit batches are atomic on disk.** One commit = one frame:
  `u32 frame_len | u64 txn_id | u32 nrecords | records… | u32 crc32`
  (all big-endian; the CRC covers everything after `frame_len`). COMMIT
  does `write_all` + `fsync` **before** publishing to the in-memory
  database and before replying to the client. Recovery stops at the first
  undecodable frame, so a torn tail from a crash mid-write can only ever
  drop a partial batch — whole committed batches replay atomically.
- **Diff-at-commit.** Explicit transactions still commit by swapping the
  session's working copy into the shared database (v0.3); at COMMIT the
  server diffs the pre-commit database against the working copy and logs
  the delta. Each delta is relative to the then-current committed state,
  so replaying deltas in order reproduces every published state, even
  under last-writer-wins concurrency. Autocommit statements derive their
  records from the statement itself while holding the db lock (INSERT
  logs the appended row suffix — O(changed rows), no full clone).
- **A failed WAL write never becomes visible.** Autocommit executes
  against the in-memory database, then fsyncs the WAL; if the fsync
  fails, the mutation is rolled back from its before-image (INSERT
  truncates the appended suffix, CREATE drops the new table, DROP
  restores the old one) and the client gets `58000` (`system_error`).
  Explicit COMMIT diffs first and publishes only after a successful
  fsync. Successful writes are never visible before they are durable.
- **Checkpoints.** `CHECKPOINT` (new SQL statement, rejected inside an
  explicit transaction with `25001`) writes a full database image to
  `checkpoint.dat.tmp`, fsyncs it, atomically renames it over
  `checkpoint.dat`, fsyncs the data directory, then resets `wal.log` to
  a fresh generation. Crash-safe at every step: a crash before the
  rename leaves the old checkpoint; between rename and WAL reset, the
  old WAL replays but already-checkpointed frames are skipped by LSN;
  during the WAL reset, an empty/torn file becomes a fresh generation.
- **Logical sequence numbers across WAL generations.**
  `wal.log` starts with a 16-byte header (`"RGSWAL01"` + `u64 base_lsn`);
  a frame's LSN is `base_lsn + (physical_offset − 16)`. The checkpoint
  stores the logical `wal_end` it covers, and the post-checkpoint WAL
  generation starts its `base_lsn` exactly there — so recovery replays
  precisely the frames with `lsn >= wal_end` no matter how many
  checkpoints truncated the physical file before them. (Comparing a
  stored pre-truncation *physical* offset against a post-truncation file
  length would silently skip or misread the new generation — the classic
  bug this scheme exists to avoid.)
- **Crash recovery at startup.** Loads the checkpoint (a present but
  undecodable `checkpoint.dat` refuses startup loudly rather than
  silently losing data), then replays WAL frames after it. Uncommitted
  transactions never reached the WAL — they die with the process, as
  they should. ROLLBACK needs no undo logging for the same reason.
- **Data directory.** `./rustgres-data` by default; override with
  `--data-dir PATH` (or `--data-dir=PATH`) or the `RUSTGRES_DATA_DIR`
  environment variable. A missing/empty directory starts an empty
  database. Layout: `wal.log` + `checkpoint.dat` (+ `.tmp` transiently).
- **`SO_REUSEADDR` on the listener** (`src/net.rs`): restart-after-`kill
  -9` must rebind `127.0.0.1:5433` even with old connections in
  `TIME_WAIT`. Pure std has no `setsockopt`, so on Linux/x86_64 the
  socket is built with raw syscalls (`socket`/`setsockopt`/`bind`/
  `listen` via inline `asm!`) and wrapped in a `TcpListener`; other
  platforms fall back to `TcpListener::bind`.

Known v0.4 deviations/limitations (all documented, none silent):
**one fsync per commit, no group commit** — `insert` throughput drops
229 → 144 qps vs v0.3, honestly measured in `benches/BASELINE.md`;
this is the price of durability, not a regression to optimize away by
weakening it. **No `pg_wal` segment files / archival / point-in-time
recovery** — one flat `wal.log`, truncated at each checkpoint; there is
no replay-to-a-timestamp. **No checksums on the checkpoint image**
(corruption → loud startup refusal, not silent repair). **Full-database
images per checkpoint** (O(database) write each time — fine for now).
WAL records are **logical, not physical**: no page layout to keep stable.
`CHECKPOINT` takes the database lock for the whole procedure (writers
block briefly — no concurrent checkpointing yet).

## What v0.1 implements

- **TCP server** on `127.0.0.1:5433` (not 5432, to avoid clashing with a real
  Postgres), one `std::thread` per connection, all sharing one in-memory
  database behind a `Mutex`.
- **Wire protocol 3.0 (simple query protocol):**
  - Startup handshake: handles `SSLRequest` (replies `'N'`, then reads the
    real startup packet) and protocol `196608`.
  - `AuthenticationOk`, 7 `ParameterStatus` pairs (`server_version=16.0`,
    `server_encoding=UTF8`, `client_encoding=UTF8`, `DateStyle=ISO, MDY`,
    `TimeZone=UTC`, `integer_datetimes=on`,
    `standard_conforming_strings=on`), `BackendKeyData`, `ReadyForQuery('I')`.
  - Message loop: `Q` (Query), `X` (Terminate), `H` (Flush), `S` (Sync →
    `ReadyForQuery`); anything else → `ErrorResponse` (`0A000`).
  - Responses: `RowDescription` (`'T'`, OIDs INT4=23, TEXT=25, BOOL=16,
    FLOAT8=701, text format), `DataRow` (`'D'`, `-1` length for NULL),
    `CommandComplete` (`'C'`), `ErrorResponse` (`'E'` with `S`/`V`/`C`
    SQLSTATE/`M` fields), `EmptyQueryResponse` (`'I'`) for blank queries.
- **SQL subset** (v0.3: a `Query` message may hold several `;`-separated
  statements — split quote/comment-aware; outside a transaction each is
  its own implicit transaction, statements after an error are skipped):
  - `CREATE TABLE name (col TYPE [, ...])` — `INT`/`INTEGER`, `TEXT`,
    `BOOL`/`BOOLEAN`, `REAL`/`FLOAT`/`FLOAT8`/`DOUBLE`
  - `INSERT INTO name [(col, ...)] VALUES (v, ...), (...), ...`
  - `SELECT [* | col [, ...]] FROM name [WHERE col = literal [AND ...]] [LIMIT n]`
  - `SELECT <literals>` with no `FROM` (e.g. `SELECT 1` → one row, `?column?`)
  - `DROP TABLE [IF EXISTS] name`
  - Literals: integers, floats, `'single-quoted'` strings with `''` escape,
    `TRUE`/`FALSE`, `NULL`. Keywords case-insensitive; identifiers fold to
    lowercase.
- **Storage:** in-memory `HashMap<String, Table>`; values sent in text format
  (bools as `t`/`f`, like psql).
- **Errors** with real SQLSTATEs: `42601` syntax, `42P01` undefined table,
  `42P07` duplicate table, `42703`/`42701` column errors, `42804`
  datatype mismatch, `42883` bad operator, `2201W` negative LIMIT.

## What v0.3 adds (transactions)

- **Statements:** `BEGIN` (`START TRANSACTION`), `COMMIT` (`END`),
  `ROLLBACK` (`ABORT`), `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name`,
  `RELEASE [SAVEPOINT] name` — all accepted in both the simple (`Q`) and
  extended (`Parse`/`Bind`/`Execute`) protocols. Tags echo Postgres:
  `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RELEASE`.
- **Design: full-database transaction overlay.** On `BEGIN`, the session
  clones the entire committed database into a private working copy; every
  statement in the transaction reads and writes that copy. Other sessions
  keep seeing the last committed state. `COMMIT` swaps the working copy
  into the shared database; `ROLLBACK` discards it. Savepoints are a stack
  of `(name, database snapshot)` — `ROLLBACK TO` restores one (and stays
  in the transaction), `RELEASE` pops it.
- **Isolation:** each session's uncommitted changes are private; a
  transaction sees its own writes (plus all previously committed data).
  **Known limitation: last-writer-wins, no MVCC.** Two concurrent
  transactions that both commit overwrite each other silently — there is
  no conflict detection, no row locking, no isolation levels yet. The
  overlay makes this simple and correct for single-writer workloads; real
  MVCC is M4+ work.
- **Failed transactions:** any error inside an explicit transaction aborts
  it (Postgres semantics). Further statements — except `COMMIT`,
  `ROLLBACK`, and `ROLLBACK TO SAVEPOINT` — are rejected with `25P02`
  ("current transaction is aborted, commands ignored until end of
  transaction block"). This applies to both protocols: in the extended
  path the error sets the failed flag and the connection still discards
  until `Sync`. `ROLLBACK TO SAVEPOINT` clears the failed flag, letting a
  savepoint recover an aborted transaction.
- **`ReadyForQuery` status byte** now tracks transaction state: `'I'`
  (idle), `'T'` (in transaction), `'E'` (in a failed transaction).
- **Multi-statement simple queries:** a `Query` message may now contain
  several `;`-separated statements (split quote/comment-aware). Outside
  a transaction each statement runs in its own implicit transaction: the
  first statements commit, the failing statement is rolled back, and the
  statements after it are **not** executed (matches Postgres). Inside an
  explicit transaction the statements share the transaction, and any
  error aborts it.
- **`$N` in `INSERT`:** `INSERT INTO t VALUES ($1, $2)` now parses,
  binds, and executes (simple protocol with no params is still `42P02`
  for placeholders). A param's type is inferred from its target column
  (`INSERT INTO t(id INT) VALUES ($1)` → `$1` is `int4`); declaring a
  conflicting OID is `42804`.
- **Edge cases:** `COMMIT`/`ROLLBACK` with no open transaction are
  accepted as no-ops (Postgres warns, we stay silent); nested `BEGIN`
  inside a transaction is a no-op; `SAVEPOINT` outside a transaction is
  `25001`; `ROLLBACK TO` an unknown savepoint is `3B001`.

Known v0.3 deviations from real Postgres (all documented, none silent):
full-database clone per transaction instead of MVCC (O(database) per
`BEGIN`/savepoint — fine for now, measured in the benchmarks); no
isolation levels (`SERIALIZABLE` etc. are not parsed yet); no concurrent-
commit conflict detection (last-writer-wins); transaction-control
statements are intercepted at the session layer and never reach the
executor (so `PREPARE ... BEGIN` is a parse-time no-op by design).

## What v0.2 adds (extended query protocol)

- **Frontend messages:** `Parse` (`P`: name, query, param OIDs), `Bind`
  (`B`: portal, statement, param/result format codes, param values),
  `Describe` (`D`: `'S'`/`'P'` + name), `Execute` (`E`: portal, max-rows),
  `Close` (`C`: `'S'`/`'P'` + name), `Sync` (`S`), `Flush` (`H`).
- **Backend responses:** `ParseComplete` (`1`), `BindComplete` (`2`),
  `CloseComplete` (`3`), `ParameterDescription` (`t`),
  `RowDescription` (`T`) / `NoData` (`n`), `PortalSuspended` (`s`),
  `EmptyQueryResponse` (`I`) — plus the v0.1 `DataRow`/`CommandComplete`/
  `ErrorResponse`/`ReadyForQuery` set.
- **Prepared statements:** the SQL is parsed to the AST at `Parse` time;
  at `Bind` time `$1, $2, …` are type-checked, coerced from text format,
  and substituted. Supported param OIDs: `INT4=23`, `TEXT=25`, `BOOL=16`,
  `FLOAT8=701`; OID `0` means *infer* — from `WHERE col = $N` (the
  column's type), from the other side of `$N + <typed>`, or integer by
  default for `$N + $M`. Declared-vs-inferred conflicts and bad literals
  are `42804`/`22P02`.
- **Portals:** named and unnamed (`''`); the unnamed statement/portal is
  replaced by the next `Parse('')`/`Bind('')`. `Execute` honors max-rows:
  leftover rows suspend the portal (`PortalSuspended`); the final
  `CommandComplete` carries the *total* row count. Re-executing a finished
  portal replays its tag with no rows.
- **Error recovery:** any extended-protocol error sends `ErrorResponse`
  and the connection discards input until the next `Sync`, which always
  returns `ReadyForQuery('I')` — per the protocol spec.
- **Formats:** text (`0`) only in v0.2; requesting binary param/result
  format is `ErrorResponse` `0A000` ("binary format not yet supported").
- **Expressions:** the SELECT list now supports `$N`, column refs,
  literals, `+`, and parentheses (e.g. `SELECT $1 + $2`), and `$N` works
  in `WHERE col = $N` comparisons.
- The simple-query (`Q`) path is byte-for-byte unchanged.

Known v0.2 deviations from real Postgres (all documented, none silent):
table/column resolution happens at `Bind`/`Describe`/`Execute` rather
than at `Parse`; a param with no inferable context defaults to `text`
(`$N + $M` defaults to `int4`); re-executed portals see current table
data (no snapshot); `SELECT $1` over the *simple* protocol is `42P02`
instead of v0.1's `42601`; closing a nonexistent statement/portal is
silently ignored (matches PG).

## Layout

```
src/
  main.rs      TCP accept loop, one thread per connection; data-dir + CLI args
  net.rs       listening socket with SO_REUSEADDR (raw syscalls, Linux/x86_64)
  protocol.rs  message framing: startup/Message read, Int16/Int32/CString, builders
  server.rs    startup handshake + simple-protocol message dispatch loop
  repl.rs      replication protocol: walsender, slots, logical decoding (v0.13)
  sql.rs       tokenizer + recursive-descent parser → AST
  exec.rs      executor: AST → in-memory storage, SQLSTATE errors
  storage.rs   HashMap tables, Value/ColType, text-format encoding
  wal.rs       write-ahead log, checkpoints, crash recovery (v0.4)
tests/
  protocol_test.py   raw-socket handshake + simple-protocol tests (v0.1)
  protocol_test2.py  raw-socket extended-protocol tests (v0.2)
  protocol_test3.py  raw-socket transaction tests (v0.3)
  protocol_test4.py  raw-socket durability tests: kill -9 + restart (v0.4)
  protocol_test5.py  MVCC + isolation level tests (v0.5)
  protocol_test6.py  query engine: JOINs/subqueries/aggregates (v0.6)
  protocol_test7.py  types, casts, operators, built-ins (v0.7)
  protocol_test8.py  B-tree indexes, planner, ANALYZE/EXPLAIN (v0.8)
  protocol_test9.py  views, sequences, GRANT/REVOKE, privileges (v0.9)
  protocol_test10.py window functions, CTEs, UPSERT, FK cascades (v0.10)
  protocol_test11.py roles, ACLs, row-level locks, advisory locks (v0.11)
  protocol_test12.py concurrency stress, soak, wire hardening (v0.12)
  protocol_test13.py replication protocol, logical decoding (v0.13)
  protocol_test14.py conformance burn-down: txn syntax (v0.15), 58 checks
  protocol_test16.py functions, cursors, TRUNCATE (v0.16), 90 checks
```

## How to run

```bash
cd ~/workspace/rustgres
cargo run        # listens on 127.0.0.1:5433, data in ./rustgres-data

# data directory: --data-dir wins, then $RUSTGRES_DATA_DIR, then ./rustgres-data
cargo run -- --data-dir /tmp/rgdata
RUSTGRES_DATA_DIR=/tmp/rgdata cargo run
```

## How to test

```bash
# terminal 1
cargo run
# terminal 2 (fresh server per suite — the suites create tables; the v0.4
# suite manages its own servers and data dirs, just needs a free 5433)
python3 tests/protocol_test.py   # v0.1: simple protocol, 40 checks
python3 tests/protocol_test2.py  # v0.2: extended protocol, 91 checks
python3 tests/protocol_test3.py  # v0.3: transactions, 93 checks
python3 tests/protocol_test4.py  # v0.4: durability, 35 checks
python3 tests/protocol_test5.py  # v0.5: MVCC + isolation, 85 checks
python3 tests/protocol_test6.py  # v0.6: query engine, 79 checks (own
                                 # ports per test; needs 55434+ free)
python3 tests/protocol_test7.py  # v0.7: types/casts/operators/built-ins,
                                 # 198 checks
python3 tests/protocol_test8.py  # v0.8: indexes/planner/ANALYZE/EXPLAIN,
                                 # 93 checks
python3 tests/protocol_test9.py  # v0.9: views/sequences/privileges, 84 checks
python3 tests/protocol_test10.py # v0.10: window/CTEs/UPSERT/FK, 70 checks
python3 tests/protocol_test11.py # v0.11: roles/ACLs/locks, 118 checks
python3 tests/protocol_test12.py # v0.12: concurrency/soak/hardening, 65 checks
python3 tests/protocol_test13.py # v0.13: replication/logical decoding,
                                 # 57 checks
python3 tests/protocol_test14.py # v0.14/v0.15: conformance burn-down,
                                 # 58 checks
python3 tests/protocol_test16.py # v0.16: functions/cursors/TRUNCATE,
                                 # 90 checks
```

The v0.2 test does the extended-protocol dance with raw sockets and asserts
on the actual wire bytes: unnamed `Parse`/`Bind`/`Describe`/`Execute`/`Sync`
for `SELECT $1 + $2`; OID-0 type inference (`WHERE id = $1` → int4,
`$1 + $2` → int4); a named prepared statement reused across two `Bind`s
with different params; `Describe` on statement (→ `ParameterDescription` +
`RowDescription`) and on portal (→ `RowDescription` only);
`Execute` with max-rows=1 → `PortalSuspended`, then resume, final
`CommandComplete` with the total count; error mid-sequence → `Sync`
recovers to `ReadyForQuery`; discarded input during the error state;
`42804`/`22P02` param errors; binary format → `0A000`; empty-query
`Parse`/`Execute` → `EmptyQueryResponse`; `Close` portal → `34000` on
re-`Execute`; unnamed-statement replacement; `Close` + re-`Parse`;
`NULL` params. The v0.3 suite asserts the transaction wire behavior:
own-writes-visible / others-don't-see, `COMMIT` publishes, `ROLLBACK`
discards, savepoint partial rewind, `25P02` + `ReadyForQuery` `'E'`,
savepoint recovery from abort, implicit-txn atomicity per statement,
DDL rollback, and extended-protocol `Parse`/`Bind`/`Execute` inside a
transaction (including `$N` in `INSERT` and inference from a
transaction-created table). The v0.1/v0.2 suites are re-run to prove no
regressions.
The v0.4 suite drives the server itself: it starts `rustgres` with a
fresh temp data dir, runs SQL over raw sockets, `kill -9`s the server at
chosen moments (mid-transaction, right after a COMMIT ack, after a
checkpoint), restarts it against the same data dir, and asserts over the
wire that committed data survived and uncommitted data did not. It
covers: committed INSERTs surviving `kill -9`; uncommitted transaction
and uncommitted DDL disappearing; committed CREATE/DROP surviving; all
200 acked commits durable; `CHECKPOINT` snapshotting + resetting the WAL
to a fresh 16-byte generation header; writes after (two) checkpoints
replaying via logical LSNs; a 1500-row WAL replaying with no checkpoint;
`ROLLBACK`ed data absent; `CHECKPOINT` inside a transaction → `25001`;
and nested data-dir auto-creation. The v0.5 suite covers MVCC and
isolation over raw sockets with concurrent connections: uncommitted /
aborted invisibility, own-writes-visible, `READ COMMITTED` fresh
snapshots (phantoms visible), `REPEATABLE READ` stable snapshots (no
phantoms), `40001` on `REPEATABLE READ` write conflicts and on
lost-update races, `SERIALIZABLE` as snapshot isolation (write skew is
*not* prevented — asserted), savepoint rollback of MVCC writes,
`UPDATE`/`DELETE` basics, `VACUUM [VERBOSE]` reclamation including
snapshot pinning (0 versions reclaimed while a `REPEATABLE READ`
snapshot is open, all reclaimed after), `VACUUM`/`CHECKPOINT` rejected
inside transactions (`25001`), isolation-level syntax
(`BEGIN ISOLATION LEVEL ...`), extended-protocol `UPDATE` inside a
transaction, crash recovery of `UPDATE`/`DELETE` chains (`kill -9` with
mixed committed/uncommitted writes), `CHECKPOINT` with an open
transaction, and `ORDER BY` (direction, null placement, positional,
multi-term, `LIMIT`-after-sort, error codes).
If you have a real `psql` client handy:

```bash
psql -h 127.0.0.1 -p 5433 -U postgres -c 'select 1'
```

Note: `psql -c` uses the simple query protocol, so it exercises the v0.1
path; the v0.2 extended protocol is what real drivers (pgx, JDBC,
psycopg, …) speak.

## Benchmarking

`benches/bench.py` is a raw-socket wire-protocol benchmark driver (stdlib
only): `SELECT 1` qps, 10k-row scan qps, batched 1000-row `INSERT`s, and an
extended-protocol prepared-statement loop, each with qps + p50/p99.

```bash
cargo build && ./target/debug/rustgres &  # terminal 1
python3 benches/bench.py --seconds 5      # terminal 2
```

`benches/profile.sh` runs the server under valgrind (`callgrind` + `dhat`)
while driving a fixed workload; results and reading notes live in
`benches/profiles/`. Baselines are recorded in `benches/BASELINE.md` —
the profile → optimize loop starts there.

## Roadmap to parity (Postgres 19)

- **M1 — Simple protocol:** handshake, `Q`/`X`, SQL subset, in-memory
  storage. ✅ done (v0.1)
- **M2 — Extended query protocol:** `Parse`/`Bind`/`Describe`/`Execute`/
  `Close`, prepared statements + portals, parameter type OIDs, text-format
  params, `+` expressions. ✅ done (v0.2)
- **M3 — Transactions:** `BEGIN`/`COMMIT`/`ROLLBACK`, `SAVEPOINT`,
  aborted-transaction state, `I`/`T`/`E` status, full-database transaction
  overlay (documented last-writer-wins limitation — MVCC deferred).
  ✅ done (v0.3)
- **M4 — Persistence:** write-ahead log + checkpoints/snapshots, crash
  recovery, `fsync` discipline. ✅ done (v0.4)
- **M5 — Types & expressions:** `NUMERIC`, `TIMESTAMP`/`DATE`/`INTERVAL`,
  `JSONB` + operators, arrays, `UUID`, casts, a real expression/operator engine
  (`>`, `<`, `LIKE`, `IN`, arithmetic). Partially done: v0.6 brought the
  expression engine a long way (general predicates, `IN`/`EXISTS`, aggregates,
  `GROUP BY`/`HAVING`/`ORDER BY`) — the remaining work is types (`NUMERIC`,
  timestamps, `JSONB`, arrays, `UUID`), casts, and string/pattern operators.
- **M6 — Indexes & planner:** B-tree indexes, `CREATE INDEX`, cost-based-ish
  planning, `EXPLAIN`. ← the big next step: joins are nested-loop only and
  every `WHERE` is a full version-chain scan (see `benches/BASELINE.md`).
- **M7 — Catalog & tooling:** `pg_catalog` / `information_schema`, `COPY
  FROM/TO`, `LISTEN`/`NOTIFY`, sequence/`SERIAL`, views.
- **M8 — Security:** real auth (md5 → SCRAM-SHA-256), roles, `GRANT`/
  privileges, SSL.
- **M9+ — The long tail:** joins ✅ (v0.6: INNER/LEFT/CROSS + derived
  tables; hash join still to come), subqueries ✅ (v0.6: scalar/IN/EXISTS,
  correlated; no LATERAL yet), CTEs, window functions,
  constraints/FKs, triggers, rules, replication protocol, partitioning…
