# rustgres v0.3 — "it takes transactions"

A from-scratch PostgreSQL-compatible database server written in pure Rust —
**zero external crates**, so it builds offline with plain `cargo build`.

Milestone 3 of the road to Postgres 19 feature parity. v0.3 adds
**transactions**: `BEGIN`/`COMMIT`/`ROLLBACK` with full ACID-ish
isolation-via-snapshot, `SAVEPOINT`/`ROLLBACK TO`/`RELEASE`, the aborted-
transaction state (`25P02`) with `ReadyForQuery` status bytes `I`/`T`/`E`,
multi-statement simple-query strings (each statement atomic), and `$N`
parameters in `INSERT ... VALUES`.

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
  main.rs      TCP accept loop, one thread per connection
  protocol.rs  message framing: startup/Message read, Int16/Int32/CString, builders
  server.rs    startup handshake + simple-protocol message dispatch loop
  sql.rs       tokenizer + recursive-descent parser → AST
  exec.rs      executor: AST → in-memory storage, SQLSTATE errors
  storage.rs   HashMap tables, Value/ColType, text-format encoding
tests/
  protocol_test.py   raw-socket handshake + simple-protocol tests (v0.1)
  protocol_test2.py  raw-socket extended-protocol tests (v0.2)
  protocol_test3.py  raw-socket transaction tests (v0.3)
```

## How to run

```bash
cd ~/workspace/rustgres
cargo run        # listens on 127.0.0.1:5433
```

## How to test

```bash
# terminal 1
cargo run
# terminal 2 (fresh server per suite — the suites create tables)
python3 tests/protocol_test.py   # v0.1: simple protocol, 40 checks
python3 tests/protocol_test2.py  # v0.2: extended protocol, 91 checks
python3 tests/protocol_test3.py  # v0.3: transactions, 93 checks
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
  recovery, `fsync` discipline. ← next
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
