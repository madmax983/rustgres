# rustgres v0.4 — "carved in stone"

A from-scratch PostgreSQL-compatible database server written in pure Rust —
**zero external crates**, so it builds offline with plain `cargo build`.

Milestone 4 of the road to Postgres 19 feature parity. v0.4 adds
**durability**: a write-ahead log for committed DML and DDL, `fsync` on
every commit, checkpoints with WAL truncation, and crash recovery that
replays committed batches and drops uncommitted ones. Kill `-9` the
server mid-transaction and restart it — committed data is there,
uncommitted data is not.

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
  sql.rs       tokenizer + recursive-descent parser → AST
  exec.rs      executor: AST → in-memory storage, SQLSTATE errors
  storage.rs   HashMap tables, Value/ColType, text-format encoding
  wal.rs       write-ahead log, checkpoints, crash recovery (v0.4)
tests/
  protocol_test.py   raw-socket handshake + simple-protocol tests (v0.1)
  protocol_test2.py  raw-socket extended-protocol tests (v0.2)
  protocol_test3.py  raw-socket transaction tests (v0.3)
  protocol_test4.py  raw-socket durability tests: kill -9 + restart (v0.4)
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
and nested data-dir auto-creation.
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
  (`>`, `<`, `LIKE`, `IN`, arithmetic, aggregates, `GROUP BY`/`ORDER BY`).
- **M6 — Indexes & planner:** B-tree indexes, `CREATE INDEX`, cost-based-ish
  planning, `EXPLAIN`.
- **M7 — Catalog & tooling:** `pg_catalog` / `information_schema`, `COPY
  FROM/TO`, `LISTEN`/`NOTIFY`, sequence/`SERIAL`, views.
- **M8 — Security:** real auth (md5 → SCRAM-SHA-256), roles, `GRANT`/
  privileges, SSL.
- **M9+ — The long tail:** joins, subqueries, CTEs, window functions,
  constraints/FKs, triggers, rules, replication protocol, partitioning…
