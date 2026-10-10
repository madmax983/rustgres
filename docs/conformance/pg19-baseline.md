# PG19 full-schedule conformance baseline

- **Measured:** 2026-10-09, rustgres `main` + this branch (release build)
- **Suite:** PostgreSQL `REL_19_STABLE` regression schedule, pinned at upstream
  `1ecc48b9` ([`tests/conformance/pg19/`](../../tests/conformance/pg19/README.md))
- **Runner:** `python3 tests/conformance/schedule_runner.py --release`
  (pg_regress semantics: one database, PG's real `test_setup.sql`, all
  suites in schedule order, no legacy masks). About 17 minutes.
- **Analysis:** `python3 tests/conformance/analyze_schedule.py pg19.json`

## Headline

| Measure | Curated gate (22 suites) | **Full schedule (236 suites)** |
|---|---:|---:|
| Statements scored | 5,247 | **51,015** |
| PASS | 4,753 (90.6%) | **22,556 (44.2%)** |
| EXPECTED-FAIL | 494 (433 of them EXPLAIN text) | 123 (regress.so C functions + OOM guard) |
| REAL-FAIL | 0 | **28,336** |
| … of which cascades | — | 11,039 (a failed `CREATE` breaks later statements) |
| … other knock-ons | — | 1,283 (aborted-transaction `25P02`, unset psql vars, missing relations) |
| **Root-cause REAL-FAILs** | — | **≈16,000** |
| Unscored | — | 60 (psql-only syntax, COPY TO STDOUT data) |

Suites by pass rate: 14 at 100%, 14 at ≥80%, 59 at 50–80%, 105 at 20–50%,
44 below 20%. The 100% suites are the scalar basics: `boolean`, `char`,
`varchar`, `text`, `int2`/`int4`/`int8`, `float4`/`float8`, `numeric`,
`delete`, `select_having`, `bitmapops`, `portals_p2`.

**How to read this.** The curated gate measures how well rustgres does on
the SQL it targets. This baseline measures how much of PostgreSQL that is.
EXPLAIN plan-text diffs, which were 88% of the curated gate's
EXPECTED-FAILs, are only **11%** of root-cause failures here (1,754). Most of
what is missing is whole features, not edge cases.

## Crash and durability bugs found (all fixed in v1.80)

These came first: they took the server down, and several took the data
directory with it. The original triggers are below; root causes and tests
are in the v1.80 commits. Re-running the schedule on v1.80 gives **zero
recovery failures and zero panics** across all 236 suites. **23,138 PASS**
(44.5%), up 582 from this baseline. Most of the gain comes from `triggers`,
which used to brick the data directory partway through.

| # | Symptom (as first seen) | Root cause | Fix |
|---|---|---|---|
| 1 | WAL recovery panic in `Database::index_insert_row`; **data directory unopenable** (`triggers`) | `DROP COLUMN` before an indexed column. The same-name index re-create hid the `DropIndex` from the WAL, so replay indexed rewritten rows through the stale definition. | Log the drop, order it before the rewrite, make replay tolerant of v1.79 logs |
| 2 | `partition.rs:193` / `:1464` index out of bounds (`alter_table`, `foreign_key`, `indexing`) | `DROP COLUMN` before the partition key left the key position stale | Shift key positions; dropping a key column now raises 42P16 |
| 3 | `partition.rs:560` "table still visible" (`copy2`, `rangefuncs`, `with`) | `CREATE`/`DROP TRIGGER` on a **temp** table looked only in the permanent catalog | `commit_table_version` versions temp tables too |
| 4 | `partition.rs:356` "child is partitioned" (`foreign_key`) | Fallout of #5 after a crash and restart | Overlap check skips stale links; #5 fixed |
| 5 | *(found while fixing #2)* After a crash, partitioned tables replayed as plain tables | Partition metadata was never WAL-logged (only checkpointed) | WAL format `RGSWAL21`; `RGSWAL20` logs still readable and upgraded on open |
| 6 | Recovery refused: "bad constraints for table `log_table`" (`triggers`) | Casts in `DEFAULT`/`CHECK` were encoded with bare multi-word type names (`timestamp without time zone`) | Quoted, typmod-preserving cast encoding; v1.79 form still decodes |
| 7 | "checkpoint.dat is corrupt (row/column count mismatch)" (`insert_conflict`, via a later checkpoint) | A multi-action `ALTER` logged every version with the final shape | Pair each op with its own version; replay and checkpoint load tolerate v1.79 debris |

Still open:
- 11 statement timeouts (30 s). They are slow plans: correlated
  `LATERAL`/sub-select queries over `tenk1` (`memoize` ×4, `join` ×2,
  `subselect`, `aggregates`, `create_index`, `partition_join`). There is
  also one harness limitation: `COPY … FROM STDIN` inside a `\;`
  multi-statement query (`copyselect`).
- `infinite_recurse` overflows the stack and aborts the process, instead
  of raising `54001` (stack depth limit exceeded).

## Biggest functional gaps (root causes, by statements affected)

These are grouped by feature. Numbers are approximate, because one
statement can need several features.

1. **JSON stack.** `jsonb` type and operators (`->`, `->>`, `@>`, `?`,
   `#>`), `jsonpath` (`jsonb_path_query*`, `@?`, `@@`), SQL/JSON
   (`JSON_VALUE`, `JSON_QUERY`, `JSON_TABLE`, `IS JSON`). ≈ 2,040 root
   failures (2,650 including cascades) across `json`, `jsonb`, `jsonpath`, `jsonb_jsonpath`, the
   `sqljson*` suites, and the JSON parts of `explain` and `copy`.
2. **Date/time completeness.** No `interval` value type, no `time`/`timetz`,
   no `DateStyle`/`TimeZone`/`IntervalStyle` GUCs, no `AT TIME ZONE`, no
   `infinity`/BC/Julian/named-zone input, and no `timestamp - timestamp`.
   ≈ 1,000 root failures (1,320 including cascades) across `interval`, `horology`, `timestamp`, `timestamptz`,
   `date`, `time`, `timetz`.
3. **Range/multirange types.** ≈ 560 root failures (1,440 including
   cascades) across `rangetypes`,
   `multirangetypes`, `without_overlaps`, and parts of `partition_prune`
   and `indexing`.
4. **Schemas and roles in DDL.** No `CREATE SCHEMA`/`search_path`, no
   `SET/RESET SESSION AUTHORIZATION` (490 statements alone), no
   `has_*_privilege()`, no `CREATE POLICY` / row-level security, no
   `COMMENT ON`. These cascade heavily: many suites switch roles or
   schemas in setup.
5. **DDL breadth.** `ALTER` for INDEX, VIEW, TYPE, SCHEMA, OPERATOR and
   publications (only TABLE, SEQUENCE, DOMAIN, FUNCTION and ROLE parse);
   several `ALTER TABLE` actions (`ALTER COLUMN … TYPE`, `SET NOT NULL`
   forms, `ATTACH/DETACH PARTITION` variants); generated and identity
   columns (141); `CREATE RULE` (99); `REINDEX`; `UNLOGGED`; `COLLATE`
   clauses (≈ 200); `CREATE TYPE … AS ENUM / RANGE`.
6. **Full-text search.** `tsvector`/`tsquery`, `to_tsvector`, `ts_rank`,
   dictionaries. ≈ 475 root failures (770 including cascades) across `tsearch`, `tstypes`, `tsdicts`.
7. **Catalog and introspection.** `pg_proc`, `pg_type`, `pg_constraint`,
   `pg_index`, `pg_operator`, `pg_get_viewdef`, `pg_input_is_valid` /
   `pg_input_error_info` (263; PG uses them to test every input
   function), and `pg_stat_*` views. These decide whether psql `\d` and
   ORMs work.
8. **Other types.** Geometric (`point`, `box`, `path`, …), `money`, `inet`/
   `cidr`/`macaddr`, `xml`, `oid` as a column type, and the polymorphic
   pseudo-types (`anyelement`, `anyarray`) in function signatures.
9. **Missing GUCs (577).** `client_min_messages` (134) is the big one: many
   suites set it first, so they fail early.
10. **Correctness, not missing features.** 1,157 wrong results and 566 cases
    where invalid input was **accepted**: constraint violations not raised
    on `INSERT`/`ALTER TABLE`, deferred constraints not checked at
    `COMMIT` (26), and `CREATE STATISTICS` validation. These are the
    closest thing here to "minor correctness", and they are worth a
    focused pass, because silent acceptance of bad data is worse than a
    missing feature.

## Harness notes

- Each failure is classified once, by its *first* symptom. "Wrong result"
  includes plan-dependent row orders only when the query has a top-level
  `ORDER BY`. Otherwise rows are compared as multisets.
- Cascade attribution is name-based (`\b<name>\b` over statement text), so
  a cascade can occasionally be attributed to the wrong failed object. The
  total REAL-FAIL count does not depend on that attribution.
- The data dir that would not reopen (bug #1) is reproducible: run the
  schedule through `triggers`, then restart the server on the same
  directory.

## Per-suite results

| suite | PASS | EXPECTED-FAIL | REAL-FAIL | cascade | pass % |
|---|---:|---:|---:|---:|---:|
| test_setup | 31 | 1 | 34 | 15 | 47 |
| boolean | 98 | 0 | 0 | 0 | 100 |
| char | 32 | 0 | 0 | 0 | 100 |
| name | 39 | 0 | 1 | 0 | 98 |
| varchar | 22 | 0 | 0 | 0 | 100 |
| text | 74 | 0 | 0 | 0 | 100 |
| int2 | 76 | 0 | 0 | 0 | 100 |
| int4 | 101 | 0 | 0 | 0 | 100 |
| int8 | 174 | 0 | 0 | 0 | 100 |
| oid | 10 | 0 | 27 | 16 | 27 |
| float4 | 100 | 0 | 0 | 0 | 100 |
| float8 | 184 | 0 | 0 | 0 | 100 |
| bit | 13 | 0 | 119 | 46 | 10 |
| numeric | 1059 | 0 | 0 | 0 | 100 |
| txid | 15 | 0 | 32 | 9 | 32 |
| uuid | 19 | 0 | 56 | 6 | 25 |
| enum | 30 | 0 | 141 | 122 | 18 |
| money | 21 | 0 | 92 | 55 | 19 |
| rangetypes | 56 | 0 | 372 | 234 | 13 |
| pg_lsn | 13 | 0 | 18 | 0 | 42 |
| regproc | 32 | 0 | 104 | 0 | 24 |
| strings | 576 | 0 | 2 | 0 | 100 |
| md5 | 0 | 0 | 14 | 0 | 0 |
| numerology | 72 | 0 | 17 | 0 | 81 |
| point | 15 | 0 | 28 | 25 | 35 |
| lseg | 4 | 0 | 12 | 9 | 25 |
| line | 12 | 0 | 23 | 11 | 34 |
| box | 18 | 0 | 83 | 76 | 18 |
| path | 5 | 0 | 18 | 13 | 22 |
| polygon | 16 | 0 | 46 | 40 | 26 |
| circle | 6 | 0 | 16 | 15 | 27 |
| date | 62 | 0 | 209 | 0 | 23 |
| time | 9 | 0 | 35 | 15 | 20 |
| timetz | 12 | 0 | 45 | 21 | 21 |
| timestamp | 27 | 0 | 150 | 105 | 15 |
| timestamptz | 41 | 0 | 363 | 112 | 10 |
| interval | 239 | 0 | 207 | 49 | 54 |
| inet | 18 | 0 | 98 | 80 | 16 |
| macaddr | 2 | 0 | 33 | 28 | 6 |
| macaddr8 | 15 | 0 | 56 | 44 | 21 |
| multirangetypes | 63 | 0 | 575 | 210 | 10 |
| geometry | 11 | 0 | 151 | 147 | 7 |
| horology | 84 | 0 | 315 | 18 | 21 |
| tstypes | 6 | 0 | 232 | 0 | 3 |
| regex | 51 | 0 | 53 | 0 | 49 |
| type_sanity | 0 | 2 | 66 | 1 | 0 |
| opr_sanity | 0 | 14 | 117 | 0 | 0 |
| misc_sanity | 0 | 0 | 5 | 0 | 0 |
| comments | 5 | 0 | 1 | 0 | 83 |
| expressions | 33 | 0 | 46 | 23 | 42 |
| unicode | 2 | 0 | 27 | 0 | 7 |
| xid | 21 | 0 | 63 | 14 | 25 |
| mvcc | 8 | 0 | 3 | 0 | 73 |
| database | 6 | 0 | 10 | 0 | 38 |
| stats_import | 66 | 0 | 301 | 292 | 18 |
| pg_ndistinct | 36 | 0 | 39 | 0 | 48 |
| pg_dependencies | 44 | 0 | 49 | 0 | 47 |
| oid8 | 12 | 0 | 37 | 21 | 24 |
| encoding | 24 | 17 | 34 | 3 | 32 |
| euc_kr | 0 | 0 | 2 | 0 | 0 |
| copy | 104 | 0 | 85 | 11 | 55 |
| copyselect | 23 | 0 | 11 | 0 | 68 |
| copydml | 23 | 0 | 36 | 16 | 39 |
| copyencoding | 4 | 0 | 23 | 0 | 15 |
| insert | 380 | 0 | 6 | 0 | 98 |
| insert_conflict | 220 | 0 | 127 | 18 | 63 |
| create_function_c | 3 | 0 | 2 | 0 | 60 |
| create_misc | 45 | 0 | 43 | 21 | 51 |
| create_operator | 56 | 0 | 39 | 13 | 59 |
| create_procedure | 38 | 0 | 71 | 46 | 35 |
| create_table | 237 | 0 | 90 | 29 | 72 |
| create_type | 36 | 7 | 52 | 35 | 38 |
| create_schema | 21 | 0 | 7 | 2 | 75 |
| create_index | 281 | 0 | 401 | 182 | 41 |
| create_index_spgist | 14 | 0 | 188 | 187 | 7 |
| create_view | 127 | 1 | 179 | 39 | 41 |
| index_including | 32 | 0 | 103 | 79 | 24 |
| index_including_gist | 5 | 0 | 45 | 38 | 10 |
| create_aggregate | 24 | 0 | 36 | 20 | 40 |
| create_function_sql | 50 | 0 | 128 | 67 | 28 |
| create_cast | 8 | 0 | 16 | 13 | 33 |
| constraints | 333 | 0 | 262 | 146 | 56 |
| triggers | 0 | 0 | 1 | 0 | 0 |
| select | 77 | 0 | 14 | 2 | 85 |
| inherit | 586 | 0 | 358 | 85 | 62 |
| typed_table | 17 | 0 | 15 | 4 | 53 |
| vacuum | 211 | 0 | 127 | 28 | 62 |
| drop_if_exists | 86 | 0 | 75 | 5 | 53 |
| updatable_views | 502 | 0 | 615 | 215 | 45 |
| roleattributes | 16 | 0 | 64 | 48 | 20 |
| create_am | 37 | 0 | 105 | 33 | 26 |
| hash_func | 6 | 0 | 38 | 3 | 14 |
| errors | 86 | 0 | 1 | 0 | 99 |
| infinite_recurse | 2 | 0 | 1 | 0 | 67 |
| sanity_check | 1 | 0 | 2 | 0 | 33 |
| select_into | 27 | 0 | 40 | 20 | 40 |
| select_distinct | 88 | 0 | 13 | 1 | 87 |
| select_distinct_on | 15 | 0 | 8 | 0 | 65 |
| select_implicit | 42 | 0 | 2 | 0 | 95 |
| select_having | 23 | 0 | 0 | 0 | 100 |
| subselect | 305 | 1 | 122 | 3 | 71 |
| union | 152 | 0 | 61 | 2 | 71 |
| case | 61 | 0 | 3 | 1 | 95 |
| join | 693 | 10 | 304 | 9 | 69 |
| aggregates | 298 | 0 | 288 | 50 | 51 |
| transactions | 431 | 0 | 2 | 1 | 100 |
| random | 20 | 0 | 48 | 5 | 29 |
| portals | 247 | 0 | 107 | 1 | 70 |
| arrays | 199 | 0 | 318 | 14 | 38 |
| btree_index | 87 | 0 | 68 | 12 | 56 |
| hash_index | 95 | 0 | 12 | 2 | 89 |
| update | 159 | 0 | 127 | 12 | 56 |
| delete | 10 | 0 | 0 | 0 | 100 |
| namespace | 10 | 0 | 31 | 11 | 24 |
| prepared_xacts | 21 | 0 | 75 | 10 | 22 |
| brin | 28 | 0 | 49 | 34 | 36 |
| gin | 22 | 0 | 37 | 7 | 37 |
| gist | 18 | 0 | 66 | 61 | 21 |
| spgist | 9 | 0 | 22 | 16 | 29 |
| privileges | 558 | 0 | 855 | 252 | 39 |
| init_privs | 0 | 0 | 4 | 0 | 0 |
| security_label | 23 | 0 | 5 | 2 | 82 |
| collate | 45 | 0 | 101 | 80 | 31 |
| matview | 48 | 0 | 133 | 100 | 27 |
| lock | 80 | 2 | 49 | 8 | 61 |
| replica_identity | 32 | 0 | 44 | 24 | 42 |
| rowsecurity | 366 | 0 | 944 | 178 | 28 |
| object_address | 39 | 0 | 48 | 24 | 45 |
| tablesample | 19 | 0 | 39 | 5 | 33 |
| groupingsets | 98 | 2 | 119 | 36 | 45 |
| drop_operator | 0 | 0 | 12 | 0 | 0 |
| password | 36 | 0 | 17 | 0 | 68 |
| identity | 87 | 0 | 184 | 100 | 32 |
| generated_stored | 153 | 0 | 319 | 237 | 32 |
| join_hash | 26 | 0 | 255 | 91 | 9 |
| brin_bloom | 15 | 0 | 25 | 18 | 38 |
| brin_multi | 89 | 0 | 83 | 39 | 52 |
| create_table_like | 71 | 0 | 108 | 71 | 40 |
| alter_generic | 139 | 2 | 192 | 82 | 42 |
| alter_operator | 19 | 0 | 46 | 3 | 29 |
| misc | 18 | 4 | 39 | 24 | 30 |
| async | 3 | 0 | 8 | 0 | 27 |
| dbsize | 16 | 0 | 9 | 0 | 64 |
| merge | 339 | 0 | 233 | 119 | 59 |
| misc_functions | 28 | 37 | 94 | 6 | 18 |
| nls | 0 | 2 | 3 | 1 | 0 |
| sysviews | 4 | 0 | 26 | 0 | 13 |
| tsrf | 47 | 0 | 29 | 2 | 62 |
| tid | 28 | 0 | 22 | 4 | 56 |
| tidscan | 16 | 0 | 33 | 4 | 33 |
| tidrangescan | 22 | 0 | 50 | 3 | 31 |
| collate.utf8 | 2 | 0 | 57 | 8 | 3 |
| collate.icu.utf8 | 108 | 0 | 632 | 414 | 15 |
| incremental_sort | 76 | 0 | 58 | 11 | 57 |
| create_role | 62 | 0 | 82 | 49 | 43 |
| without_overlaps | 169 | 0 | 489 | 430 | 26 |
| generated_virtual | 151 | 0 | 328 | 248 | 32 |
| rules | 364 | 0 | 268 | 31 | 58 |
| amutils | 2 | 0 | 8 | 5 | 20 |
| stats_ext | 250 | 0 | 639 | 489 | 28 |
| collate.linux.utf8 | 32 | 0 | 174 | 109 | 16 |
| collate.windows.win1252 | 29 | 0 | 144 | 98 | 17 |
| select_parallel | 20 | 0 | 230 | 43 | 8 |
| write_parallel | 6 | 0 | 16 | 10 | 27 |
| vacuum_parallel | 8 | 0 | 6 | 0 | 57 |
| maintain_every | 6 | 0 | 10 | 7 | 38 |
| publication | 251 | 0 | 598 | 183 | 30 |
| subscription | 86 | 3 | 138 | 27 | 38 |
| select_views | 12 | 0 | 40 | 24 | 23 |
| portals_p2 | 41 | 0 | 0 | 0 | 100 |
| foreign_key | 833 | 0 | 761 | 418 | 52 |
| dependency | 31 | 0 | 31 | 1 | 50 |
| guc | 56 | 0 | 190 | 18 | 23 |
| bitmapops | 12 | 0 | 0 | 0 | 100 |
| combocid | 50 | 0 | 12 | 0 | 81 |
| tsearch | 32 | 0 | 414 | 283 | 7 |
| tsdicts | 9 | 0 | 122 | 10 | 7 |
| foreign_data | 231 | 5 | 306 | 33 | 43 |
| window | 106 | 0 | 350 | 62 | 23 |
| xmlmap | 4 | 0 | 36 | 30 | 10 |
| functional_deps | 27 | 0 | 13 | 0 | 68 |
| advisory_lock | 8 | 0 | 30 | 0 | 21 |
| indirect_toast | 21 | 3 | 4 | 2 | 75 |
| equivclass | 34 | 0 | 62 | 41 | 35 |
| stats_rewrite | 42 | 0 | 127 | 51 | 25 |
| json | 115 | 0 | 354 | 105 | 25 |
| jsonb | 180 | 0 | 920 | 377 | 16 |
| json_encoding | 18 | 0 | 26 | 0 | 41 |
| jsonpath | 52 | 0 | 208 | 3 | 20 |
| jsonpath_encoding | 18 | 0 | 14 | 0 | 56 |
| jsonb_jsonpath | 285 | 0 | 643 | 5 | 31 |
| sqljson | 117 | 0 | 215 | 50 | 35 |
| sqljson_queryfuncs | 136 | 0 | 197 | 41 | 41 |
| sqljson_jsontable | 44 | 0 | 76 | 31 | 37 |
| plancache | 62 | 0 | 35 | 15 | 64 |
| limit | 49 | 0 | 31 | 1 | 61 |
| plpgsql | 367 | 0 | 568 | 293 | 39 |
| copy2 | 169 | 0 | 126 | 17 | 57 |
| temp | 102 | 0 | 105 | 56 | 49 |
| domain | 204 | 0 | 278 | 188 | 42 |
| rangefuncs | 151 | 0 | 277 | 178 | 35 |
| prepare | 20 | 0 | 13 | 2 | 61 |
| conversion | 17 | 3 | 34 | 7 | 31 |
| truncate | 134 | 0 | 59 | 23 | 69 |
| alter_table | 1010 | 0 | 676 | 268 | 60 |
| sequence | 148 | 0 | 113 | 43 | 57 |
| polymorphism | 126 | 0 | 310 | 234 | 29 |
| rowtypes | 112 | 0 | 125 | 48 | 47 |
| returning | 53 | 0 | 98 | 38 | 35 |
| largeobject | 47 | 0 | 80 | 60 | 37 |
| with | 140 | 0 | 161 | 63 | 47 |
| xml | 43 | 0 | 230 | 106 | 16 |
| partition_join | 420 | 0 | 220 | 19 | 66 |
| partition_prune | 450 | 0 | 359 | 58 | 56 |
| reloptions | 27 | 0 | 45 | 26 | 38 |
| hash_part | 16 | 0 | 13 | 12 | 55 |
| indexing | 364 | 0 | 259 | 72 | 58 |
| partition_aggregate | 92 | 0 | 46 | 0 | 67 |
| partition_info | 19 | 0 | 59 | 19 | 24 |
| tuplesort | 50 | 0 | 58 | 2 | 46 |
| explain | 20 | 0 | 41 | 33 | 33 |
| memoize | 55 | 0 | 27 | 16 | 67 |
| stats | 182 | 0 | 315 | 24 | 37 |
| predicate | 78 | 0 | 60 | 0 | 57 |
| numa | 0 | 0 | 2 | 0 | 0 |
| eager_aggregate | 97 | 0 | 28 | 0 | 78 |
| planner_est | 4 | 0 | 18 | 18 | 18 |
| compression | 26 | 0 | 15 | 7 | 63 |
| compression_lz4 | 44 | 0 | 23 | 9 | 66 |
| compression_pglz | 11 | 7 | 0 | 0 | 61 |
| cluster | 158 | 0 | 101 | 35 | 61 |
| oidjoins | 0 | 0 | 1 | 1 | 0 |
| event_trigger | 61 | 0 | 171 | 72 | 26 |
| event_trigger_login | 2 | 0 | 9 | 2 | 18 |
| fast_default | 201 | 0 | 94 | 53 | 68 |
| tablespace | 54 | 0 | 151 | 129 | 26 |

## REAL-FAIL categories (generated by `analyze_schedule.py`)

| category | count |
|---|---:|
| cascade | 11039 |
| unsupported syntax | 6068 |
| missing function | 2700 |
| EXPLAIN plan text | 1754 |
| wrong result | 1157 |
| missing type | 976 |
| knock-on: statement in aborted transaction | 791 |
| missing GUC | 577 |
| accepted invalid input (no error) | 566 |
| missing catalog | 564 |
| knock-on: relation missing (probable cascade) | 349 |
| unexpected error 22P02 | 264 |
| feature not supported (0A000) | 234 |
| unexpected error 42703 | 147 |
| knock-on: psql variable never set (earlier \gset failed) | 143 |
| missing operator | 134 |
| unexpected error 42P07 | 133 |
| unexpected error 23514 | 93 |
| unexpected error 42704 | 89 |
| unexpected error 22023 | 58 |
| unexpected error 42501 | 53 |
| unexpected error 3B001 | 51 |
| catalog column | 49 |
| other | 48 |
| unexpected error 2BP01 | 32 |
| unexpected error 42804 | 30 |
| unexpected error 42710 | 25 |
| unexpected error 23503 | 24 |
| server crash/timeout | 23 |
| unexpected error 42803 | 20 |
| unexpected error 22008 | 19 |
| unexpected error 26000 | 19 |
| unexpected error 23505 | 18 |
| unexpected error 42821 | 16 |
| unexpected error 42846 | 11 |
| unexpected error 42723 | 11 |
| unexpected error 42701 | 6 |
| unexpected error 42830 | 6 |
| unexpected error 42809 | 6 |
| unexpected error 42P13 | 6 |
| unexpected error 23502 | 4 |
| unexpected error 42P10 | 2 |
| unexpected error 42P16 | 2 |
| unexpected error 42712 | 2 |
| unexpected error 25001 | 2 |
| unexpected error 42883 | 2 |
| unexpected error 25006 | 2 |
| unexpected error 54001 | 2 |
| unexpected error 42P19 | 2 |
| unexpected error 21000 | 2 |
| unexpected error 2202E | 1 |
| unexpected error 22004 | 1 |
| unexpected error 42P08 | 1 |
| unexpected error 40001 | 1 |
| unexpected error 22003 | 1 |

#### unsupported syntax (top 25)

| key | count | suites |
|---|---:|---|
| `SELECT … str` | 516 | horology, interval, json, json_encoding, jsonb, jsonb_jsonpath +9 |
| `SET … authorization` | 355 | alter_generic, alter_operator, alter_table, cluster, copy2, create_role +21 |
| `SELECT … at` | 206 | arrays, compression_lz4, create_index, gin, jsonb, jsonb_jsonpath +4 |
| `CREATE … generated` | 141 | constraints, create_table_like, generated_stored, generated_virtual, identity, publication +6 |
| `RESET … authorization` | 135 | alter_generic, alter_operator, alter_table, cluster, conversion, copy2 +19 |
| `SELECT … gt` | 131 | json, jsonb, multirangetypes, rangefuncs, tstypes |
| `CREATE … collate` | 103 | alter_table, btree_index, cluster, collate, collate.icu.utf8, collate.linux.utf8 +9 |
| `ALTER PUBLICATION` | 103 | publication |
| `CREATE … policy` | 100 | copy2, event_trigger, foreign_key, merge, privileges, rowsecurity +1 |
| `SELECT … time` | 99 | horology, timestamptz |
| `CREATE … schema` | 99 | alter_generic, alter_table, collate, collate.icu.utf8, collate.linux.utf8, collate.windows.win1252 +33 |
| `CREATE … rule` | 99 | alter_table, copydml, drop_if_exists, foreign_key, insert, join +7 |
| `SELECT … collate` | 96 | aggregates, alter_table, arrays, collate, collate.icu.utf8, collate.linux.utf8 +8 |
| `ALTER … not` | 91 | aggregates, alter_table, constraints, domain, foreign_data, foreign_key +7 |
| `CREATE FUNCTION` | 74 | aggregates, brin, btree_index, create_function_sql, create_index, domain +20 |
| `SELECT … shr` | 71 | json, jsonb |
| `COPY … lparen` | 68 | copy, copy2, copydml, copyencoding, copyselect, merge +1 |
| `ALTER FOREIGN` | 67 | alter_generic, fast_default, foreign_data, subscription |
| `DROP … publication` | 63 | alter_table, object_address, publication |
| `SELECT … lparen` | 57 | bit, expressions, hash_func, horology, json, jsonb +5 |
| `REINDEX … reindex` | 55 | constraints, create_index, create_table, event_trigger, indexing, privileges +2 |
| `SELECT … qident` | 54 | collate, collate.icu.utf8, collate.linux.utf8, collate.utf8, collate.windows.win1252, create_am +2 |
| `MERGE … merge` | 53 | merge, privileges, returning, rowsecurity, rules, updatable_views |
| `ALTER SUBSCRIPTION` | 53 | subscription |
| `COMMENT … comment` | 50 | alter_table, collate.icu.utf8, collate.linux.utf8, collate.windows.win1252, constraints, conversion +10 |

#### missing function (top 25)

| key | count | suites |
|---|---:|---|
| `jsonb_path_query` | 424 | jsonb_jsonpath |
| `pg_input_error_info` | 169 | arrays, bit, box, date, geometry, inet +31 |
| `pg_input_is_valid` | 94 | arrays, bit, box, date, geometry, inet +27 |
| `ts_lexize` | 73 | tsdicts, tsearch |
| `pg_get_viewdef` | 70 | aggregates, create_view, groupingsets, rules, window |
| `jsonb_path_query_tz` | 64 | jsonb_jsonpath |
| `nummultirange` | 60 | multirangetypes |
| `numrange` | 56 | multirangetypes, rangetypes |
| `to_tsquery` | 46 | tsdicts, tsearch |
| `has_table_privilege` | 40 | privileges |
| `position` | 38 | bit |
| `websearch_to_tsquery` | 29 | tsearch |
| `to_tsvector` | 28 | json, jsonb, tsdicts, tsearch |
| `has_largeobject_privilege` | 26 | privileges |
| `pg_stat_force_next_flush` | 26 | select_parallel, stats, stats_rewrite |
| `random` | 24 | random |
| `ts_rank_cd` | 22 | tsearch, tstypes |
| `pg_partition_tree` | 21 | cluster, partition_info |
| `multirange_minus_multi` | 20 | multirangetypes |
| `array_sort` | 20 | arrays |
| `jsonb_set` | 20 | jsonb |
| `jsonb_insert` | 18 | jsonb |
| `ts_headline` | 16 | json, jsonb, tsearch |
| `xpath` | 16 | xml |
| `jsonb_typeof` | 15 | jsonb |

#### EXPLAIN plan text (top 25)

| key | count | suites |
|---|---:|---|
| `EXPLAIN` | 1754 | aggregates, alter_table, brin, brin_bloom, brin_multi, btree_index +71 |

#### wrong result (top 25)

| key | count | suites |
|---|---:|---|
| `SELECT *` | 436 | alter_table, arrays, cluster, copy, copy2, create_procedure +34 |
| `SELECT COUNT(*)` | 45 | brin_multi, btree_index, create_view, event_trigger_login, foreign_key, merge +4 |
| `SELECT SUM(UNIQUE1)` | 31 | window |
| `SELECT '2006-08-13` | 26 | guc |
| `SELECT TABLEOID::REGCLASS,` | 25 | foreign_key, generated_stored, identity, indexing, inherit, merge +3 |
| `SELECT DATE` | 15 | date |
| `SELECT T1.A,` | 14 | partition_join |
| `SELECT A,` | 11 | fast_default, foreign_key, groupingsets, partition_aggregate, updatable_views, update +1 |
| `DELETE FROM` | 11 | returning, rowsecurity |
| `SELECT CTID,CMIN,*` | 11 | combocid |
| `SELECT ID,` | 10 | copy2, rowsecurity |
| `SELECT PG_COLUMN_COMPRESSION(F1)` | 9 | compression, compression_lz4 |
| `SELECT 'A'` | 8 | regex |
| `SELECT EXTRACT(CENTURY` | 7 | date |
| `SELECT RELNAME` | 7 | create_view, guc, temp |
| `SELECT TABLEOID::REGCLASS::TEXT,` | 7 | update |
| `SELECT F1,` | 7 | rowtypes, window |
| `SELECT RELNAME,` | 7 | cluster, indexing |
| `SELECT EXTRACT(EPOCH` | 6 | date, timestamp, timestamptz |
| `SELECT EXTRACT(MILLENNIUM` | 6 | date |
| `SELECT *,` | 6 | misc, rangefuncs, rowsecurity |
| `SELECT X,` | 6 | collate.icu.utf8, window |
| `SELECT FOUR,` | 6 | window |
| `UPDATE FOO` | 6 | returning |
| `FETCH NEXT` | 5 | portals, tidscan |

#### missing type (top 25)

| key | count | suites |
|---|---:|---|
| `jsonpath` | 216 | jsonpath, jsonpath_encoding |
| `jsonb` | 144 | explain, incremental_sort, json, json_encoding, jsonb, sqljson_jsontable +1 |
| `void` | 54 | copy2, create_function_sql, create_index, dependency, matview, plancache +10 |
| `tsquery` | 53 | tstypes |
| `textmultirange` | 31 | multirangetypes |
| `int4range` | 30 | indexing, multirangetypes, partition_prune, rangetypes, stats_ext, without_overlaps |
| `json` | 29 | copy, join_hash, json, json_encoding |
| `interval` | 28 | arrays, brin_multi, interval, timestamp, timestamptz, uuid |
| `anyelement` | 28 | arrays, collate, collate.icu.utf8, collate.linux.utf8, collate.windows.win1252, create_aggregate +7 |
| `money` | 26 | create_table, money |
| `box` | 23 | box, create_index, gist, index_including, index_including_gist, matview +1 |
| `tsvector` | 20 | create_index, tsearch, tstypes |
| `anyarray` | 17 | aggregates, create_function_sql, plpgsql, polymorphism |
| `oid` | 16 | aggregates, create_type, fast_default, foreign_key, largeobject, oid +4 |
| `event_trigger` | 16 | event_trigger, event_trigger_login |
| `point` | 15 | copy, create_table_like, gist, incremental_sort, point, rowtypes +4 |
| `nummultirange` | 12 | multirangetypes |
| `record` | 12 | create_view, plpgsql, rangefuncs, rowsecurity, stats_ext |
| `inet` | 11 | alter_table, brin_multi, foreign_key, gist, inet |
| `xid8` | 10 | xid |
| `anycompatiblearray` | 10 | plpgsql, polymorphism |
| `anycompatible` | 9 | create_aggregate, multirangetypes, polymorphism, rangetypes |
| `oid8` | 9 | oid8 |
| `int2vector` | 9 | arrays |
| `oidvector` | 9 | arrays |

#### missing GUC (top 25)

| key | count | suites |
|---|---:|---|
| `client_min_messages` | 134 | alter_generic, alter_table, cluster, collate.icu.utf8, collate.linux.utf8, collate.windows.win1252 +9 |
| `datestyle` | 74 | brin_multi, date, guc, horology, interval, jsonb_jsonpath +3 |
| `row_security` | 63 | rowsecurity |
| `timezone` | 40 | event_trigger, horology, jsonb_jsonpath, rangetypes, timestamptz, timetz |
| `vacuum_cost_delay` | 35 | guc |
| `search_path` | 31 | collate.icu.utf8, create_function_sql, create_operator, expressions, fast_default, foreign_key +10 |
| `constraint_exclusion` | 16 | alter_table, generated_virtual, partition_prune, predicate, privileges, rules |
| `debug_parallel_query` | 15 | partition_join, plpgsql, privileges, select_parallel, with |
| `intervalstyle` | 14 | interval |
| `client_encoding` | 12 | collate.icu.utf8, collate.linux.utf8, collate.utf8, collate.windows.win1252, copyencoding |
| `log_min_messages` | 11 | guc |
| `enable_incremental_sort` | 7 | collate.icu.utf8, incremental_sort, partition_aggregate |
| `enable_partitionwise_aggregate` | 7 | collate.icu.utf8, eager_aggregate, partition_aggregate, partition_join |
| `maintenance_work_mem` | 6 | cluster, create_index, vacuum |
| `check_function_bodies` | 6 | create_function_sql, guc |
| `default_table_access_method` | 6 | create_am |
| `password_encryption` | 6 | password |
| `enable_material` | 6 | collate.icu.utf8, incremental_sort, memoize, stats, tuplesort |
| `max_parallel_maintenance_workers` | 5 | brin, btree_index, tuplesort, vacuum_parallel |
| `stats_fetch_consistency` | 5 | stats |
| `hash_mem_multiplier` | 4 | groupingsets, memoize |
| `icu_validation_level` | 4 | collate.icu.utf8 |
| `session_replication_role` | 4 | event_trigger, rules |
| `max_stack_depth` | 4 | json, jsonb |
| `enable_partition_pruning` | 4 | partition_prune |

#### accepted invalid input (no error) (top 25)

| key | count | suites |
|---|---:|---|
| `INSERT INTO` | 97 | alter_table, arrays, collate.icu.utf8, create_index, create_table_like, foreign_key +9 |
| `ALTER TABLE` | 66 | alter_table, constraints, create_role, create_table, create_view, domain +11 |
| `SELECT *` | 40 | alter_table, privileges, rowsecurity, rowtypes, updatable_views |
| `COMMIT;` | 26 | constraints, foreign_key, temp, without_overlaps |
| `CREATE TABLE` | 23 | constraints, create_table, create_table_like, foreign_key, indexing, privileges +1 |
| `DELETE FROM` | 20 | alter_table, foreign_key, plpgsql, privileges, rowsecurity, updatable_views |
| `CREATE STATISTICS` | 13 | stats_ext |
| `UPDATE RF_TBL_ABCD_PK` | 11 | publication |
| `DROP ROLE` | 10 | create_role, event_trigger, foreign_data, privileges |
| `CREATE INDEX` | 8 | alter_table, create_index, indexing, privileges |
| `CREATE FUNCTION` | 7 | create_function_sql, plpgsql, polymorphism, privileges |
| `DROP TABLE` | 7 | create_role, foreign_key, rowsecurity |
| `UPDATE RF_TBL_ABCD_NOPK` | 7 | publication |
| `UPDATE TESTPUB_TBL8` | 6 | publication |
| `SELECT DATE` | 5 | date |
| `ALTER DOMAIN` | 5 | domain |
| `WITH RECURSIVE` | 5 | with |
| `SELECT '1'::XID` | 4 | xid |
| `UPDATE RANGE_PARTED` | 4 | update |
| `SELECT 1` | 4 | privileges |
| `SELECT Y` | 4 | privileges |
| `CREATE DOMAIN` | 4 | domain, privileges |
| `DROP USER` | 4 | dependency, privileges |
| `UPDATE RF_TBL_ABCD_PART_PK` | 4 | publication |
| `UPDATE FK_NOTPARTITIONED_PK` | 4 | foreign_key |

#### missing catalog (top 25)

| key | count | suites |
|---|---:|---|
| `pg_proc` | 59 | btree_index, opr_sanity |
| `pg_constraint` | 58 | alter_table, cluster, constraints, foreign_data, foreign_key, indexing +2 |
| `pg_type` | 55 | alter_table, create_type, dependency, type_sanity |
| `pg_index` | 34 | alter_table, cluster, create_index, indexing, misc_sanity, opr_sanity |
| `pg_operator` | 32 | alter_operator, opr_sanity |
| `information_schema.views` | 20 | collate, collate.icu.utf8, collate.linux.utf8, collate.windows.win1252, updatable_views, xml |
| `pg_stat_io` | 20 | stats |
| `pg_prepared_statements` | 16 | guc, plancache, prepare, sysviews |
| `pg_stat_all_tables` | 15 | stats, stats_rewrite, vacuum |
| `pg_cursors` | 13 | guc, portals, sysviews |
| `pg_aggregate` | 12 | opr_sanity |
| `pg_range` | 11 | type_sanity |
| `pg_statistic_ext` | 10 | stats_ext |
| `pg_depend` | 9 | alter_operator, create_am, create_view, misc_sanity |
| `pg_collation` | 9 | collate.icu.utf8, collate.linux.utf8, collate.windows.win1252 |
| `pg_am` | 7 | create_am, opr_sanity, type_sanity |
| `pg_amop` | 7 | opr_sanity |
| `pg_stat_user_tables` | 7 | stats, vacuum |
| `pg_rules` | 7 | rules |
| `pg_publication_tables` | 7 | publication |
| `pg_cast` | 6 | opr_sanity |
| `pg_class` | 5 | groupingsets, merge, regproc, reloptions |
| `pg_database` | 5 | advisory_lock, database, prepare, stats |
| `pg_stat_slru` | 5 | stats, sysviews |
| `pg_opclass` | 4 | opr_sanity |

#### unexpected error 22P02 (top 25)

| key | count | suites |
|---|---:|---|
| `invalid date "infinity"` | 23 | date, timestamp, timestamptz |
| `invalid month "Jan"` | 9 | date |
| `invalid second "00 Europe/Moscow"` | 8 | timestamptz |
| `invalid second "00 MSK"` | 8 | timestamptz |
| `invalid date "20011227"` | 8 | horology |
| `invalid day "31 BC"` | 7 | date |
| `invalid second "00 UTC"` | 7 | timestamptz |
| `invalid time "12:00"` | 7 | horology |
| `invalid date "J2452271"` | 6 | horology |
| `invalid year "Jan"` | 5 | date |
| `invalid day "24 BC"` | 5 | date, horology, random |
| `invalid date "-infinity"` | 5 | brin_multi, date |
| `invalid date "Wed"` | 5 | timestamptz |
| `invalid date "tomorrow"` | 4 | date, horology |
| `invalid date "Jan"` | 4 | timestamptz |
| `invalid date "January 8, 1999"` | 3 | date |
| `invalid date "01/02/03"` | 3 | date |
| `invalid date "19990108"` | 3 | date |
| `invalid date "990108"` | 3 | date |
| `invalid date "1999.008"` | 3 | date |
| `invalid date "J2451187"` | 3 | date |
| `invalid date "1999 Jan 08"` | 3 | date |
| `invalid date "08 Jan 1999"` | 3 | date |
| `invalid date "Jan 08 1999"` | 3 | date |
| `invalid date "1999 08 Jan"` | 3 | date |

#### feature not supported (0A000) (top 25)

| key | count | suites |
|---|---:|---|
| `CREATE INDEX` | 51 | brin, brin_bloom, brin_multi, cluster, create_am, create_index +8 |
| `CREATE FUNCTION` | 40 | aggregates, collate.icu.utf8, collate.linux.utf8, collate.windows.win1252, create_cast, create_index +9 |
| `SELECT TO_CHAR(NOW(),` | 18 | timestamptz |
| `SELECT TO_TIMESTAMP('2011-12-18` | 16 | horology |
| `CREATE OR` | 11 | collate.icu.utf8, collate.linux.utf8, collate.windows.win1252, domain, plpgsql |
| `SELECT I,` | 8 | horology |
| `SELECT TO_TIMESTAMP('2000` | 6 | horology |
| `INSERT INTO` | 6 | arrays, insert_conflict |
| `ALTER TABLE` | 5 | alter_table, constraints, foreign_data |
| `SELECT TO_TIMESTAMP('1997` | 4 | horology |
| `COPY COPY_ENCODING_TAB` | 4 | copyencoding |
| `SELECT TO_DATE('01` | 4 | collate.linux.utf8 |
| `SELECT TO_TIMESTAMP('97/FEB/16',` | 3 | horology |
| `SELECT TO_TIMESTAMP('1985` | 2 | horology |
| `SELECT TO_TIMESTAMP('` | 2 | horology |
| `SELECT TO_DATE('2016` | 2 | horology |
| `COPY COPYTEST` | 2 | copy |
| `COPY COPYTEST2` | 2 | copy |
| `COPY PARTED_COPYTEST` | 2 | copy |
| `CREATE CAST` | 2 | plpgsql, privileges |
| `ALTER FUNCTION` | 2 | alter_generic |
| `SELECT TO_CHAR(DATE` | 2 | collate.linux.utf8 |
| `SELECT X,` | 2 | window |
| `CREATE DOMAIN` | 2 | domain |
| `TRUNCATE TRUNCATE_A` | 2 | truncate |

#### unexpected error 42703 (top 25)

| key | count | suites |
|---|---:|---|
| `column "is_updatable" does not exist` | 13 | updatable_views |
| `column "current_user" does not exist` | 13 | guc, rowsecurity |
| `column "is_insertable_into" does not exist` | 12 | updatable_views |
| `column b.tableoid does not exist` | 10 | inherit |
| `column c.tableoid does not exist` | 10 | inherit |
| `column d.tableoid does not exist` | 10 | inherit |
| `column a.tableoid does not exist` | 9 | inherit |
| `column "session_user" does not exist` | 7 | privileges |
| `column "f3" does not exist` | 4 | identity |
| `column "ctid" does not exist` | 4 | tid, tidrangescan |
| `column "current_schema" does not exist` | 3 | expressions |
| `column "f1" does not exist` | 3 | join |
| `column "g" does not exist` | 3 | tsrf |
| `column c1.relam does not exist` | 2 | type_sanity |
| `column c1.relnatts does not exist` | 2 | type_sanity |
| `column "localtimestamp" does not exist` | 2 | expressions |
| `column "one" does not exist` | 2 | groupingsets, join |
| `column hobbies_r.equipment does not exist` | 2 | misc |
| `column "two" does not exist` | 1 | numerology |
| `column a1.atttypid does not exist` | 1 | type_sanity |
| `column "current_time" does not exist` | 1 | expressions |
| `column "localtime" does not exist` | 1 | expressions |
| `column "current_catalog" does not exist` | 1 | expressions |
| `column "aa" of relation "a_star" does not exist` | 1 | create_misc |
| `column "foo" of relation "a_star" does not exist` | 1 | create_misc |

#### missing operator (top 25)

| key | count | suites |
|---|---:|---|
| `boolean - integer` | 24 | inet, jsonb |
| `boolean * integer` | 11 | multirangetypes |
| `boolean / integer` | 8 | money |
| `boolean + integer` | 8 | brin_multi, inet, window |
| `timestamp with time zone - timestamp with time zone` | 7 | horology, timestamptz |
| `"char" = text` | 6 | create_table, misc_sanity, sanity_check, type_sanity, uuid |
| `timestamp without time zone - timestamp without time zone` | 6 | horology, timestamp |
| `integer[] = text` | 5 | arrays, create_index |
| `integer[] < integer[]` | 5 | arrays |
| `tid < text` | 5 | tidrangescan |
| `boolean # integer` | 5 | jsonb |
| `pg_lsn - pg_lsn` | 4 | pg_lsn |
| `text[] = text` | 4 | arrays, create_index |
| `tid = text` | 4 | tidscan |
| `tid > text` | 4 | tidrangescan |
| `date >= text` | 3 | date, horology |
| `tid >= text` | 3 | tidrangescan |
| `pg_lsn + numeric` | 2 | pg_lsn |
| `pg_lsn - numeric` | 2 | pg_lsn |
| `pg_lsn < pg_lsn` | 1 | pg_lsn |
| `text = pg_lsn` | 1 | pg_lsn |
| `pg_lsn <> text` | 1 | pg_lsn |
| `text < pg_lsn` | 1 | pg_lsn |
| `text > pg_lsn` | 1 | pg_lsn |
| `numeric + pg_lsn` | 1 | pg_lsn |

#### unexpected error 42P07 (top 25)

| key | count | suites |
|---|---:|---|
| `relation "" already exists` | 20 | alter_table, btree_index, create_table_like, foreign_data, indexing, merge +2 |
| `relation "rw_view2" already exists` | 8 | updatable_views |
| `relation "c1" already exists` | 5 | alter_table, inherit |
| `relation "rw_view1" already exists` | 4 | updatable_views |
| `relation "pktable" already exists` | 4 | foreign_key |
| `relation "pk" already exists` | 4 | foreign_key, indexing |
| `relation "pt" already exists` | 4 | foreign_key, partition_info |
| `relation "idxpart2" already exists` | 4 | indexing |
| `relation "p1_c1" already exists` | 3 | inherit |
| `relation "t1" already exists` | 3 | alter_table, rowsecurity |
| `relation "pktable2" already exists` | 3 | foreign_key |
| `relation "fk_partitioned_fk_1" already exists` | 3 | foreign_key |
| `relation "ref" already exists` | 3 | foreign_key |
| `relation "fk1" already exists` | 3 | foreign_key |
| `relation "pp_nn_1" already exists` | 2 | constraints |
| `relation "matest1" already exists` | 2 | inherit |
| `relation "t2" already exists` | 2 | fast_default, rowsecurity |
| `relation "collate_dep_test4t" already exists` | 2 | collate.linux.utf8, collate.windows.win1252 |
| `relation "fk_partitioned_fk_2" already exists` | 2 | foreign_key |
| `relation "fk_partitioned_fk_3" already exists` | 2 | foreign_key |
| `relation "fk_partitioned_pk_6" already exists` | 2 | foreign_key |
| `relation "fk" already exists` | 2 | foreign_key |
| `relation "lt1_a_idx" already exists` | 2 | foreign_data |
| `relation "attbl" already exists` | 2 | alter_table |
| `relation "idxpart" already exists` | 2 | indexing |

#### unexpected error 23514 (top 25)

| key | count | suites |
|---|---:|---|
| `no partition of relation "part_b_10_b_20" found for row` | 15 | update |
| `no partition of relation "parted_conflict_test" found for ro` | 6 | insert_conflict |
| `no partition of relation "pitest2" found for row` | 6 | identity |
| `no partition of relation "hp" found for row` | 6 | partition_prune |
| `no partition of relation "pitest3" found for row` | 3 | identity |
| `new row for relation "pitest3_p1" violates partition constra` | 3 | identity |
| `no partition of relation "pa_target" found for row` | 3 | merge |
| `no partition of relation "fk_partitioned_fk" found for row` | 3 | foreign_key |
| `new row for relation "beta_pos" violates partition constrain` | 3 | partition_join |
| `new row for relation "pitest2_p1" violates partition constra` | 2 | identity |
| `new row for relation "inhg" violates check constraint "foo"` | 2 | create_table_like |
| `value for domain dcomptype violates check constraint "c2"` | 2 | domain |
| `no partition of relation "plt2_adv" found for row` | 2 | partition_join |
| `no partition of relation "idxpart" found for row` | 2 | indexing |
| `new row for relation "p1_c3" violates check constraint "inh_` | 1 | inherit |
| `new row for relation "invalid_check_con" violates check cons` | 1 | inherit |
| `partition "bool_rp_true_1k" would overlap partition "bool_rp` | 1 | inherit |
| `partition "bool_rp_true_2k" would overlap partition "bool_rp` | 1 | inherit |
| `new row for relation "errtst_child_fastdef" violates check c` | 1 | inherit |
| `no partition of relation "errtst_child_fastdef" found for ro` | 1 | inherit |
| `new row for relation "errtst_child_plaindef" violates check ` | 1 | inherit |
| `no partition of relation "part_def" found for row` | 1 | update |
| `no partition of relation "list_default" found for row` | 1 | update |
| `new row for relation "hpart1" violates partition constraint` | 1 | update |
| `new row for relation "hpart2" violates partition constraint` | 1 | update |

#### unexpected error 42704 (top 25)

| key | count | suites |
|---|---:|---|
| `role "public" does not exist` | 24 | create_role, event_trigger, event_trigger_login, privileges, rowsecurity, select_views |
| `role "current_user" does not exist` | 5 | init_privs, vacuum |
| `role "pg_read_all_settings" does not exist` | 5 | privileges |
| `role "regress_priv_group2" does not exist` | 5 | privileges |
| `constraint "fk_partitioned_fk_a_b_fkey" of relation "fk_part` | 5 | foreign_key |
| `role "pg_monitor" does not exist` | 3 | misc_functions |
| `role "pg_database_owner" does not exist` | 2 | privileges |
| `role "regress_dep_user" does not exist` | 2 | dependency |
| `role "regress_test_role_super" does not exist` | 2 | foreign_data |
| `constraint "onek_unique1_constraint_foo" of relation "onek" ` | 2 | alter_table |
| `constraint "notnull_tbl1_b_not_null" of relation "notnull_tb` | 1 | constraints |
| `constraint "nn_chld0" of relation "notnull_chld0" does not e` | 1 | constraints |
| `constraint "ac_aa_check" of relation "ac" does not exist` | 1 | inherit |
| `constraint "ac_check" of relation "bc" does not exist` | 1 | inherit |
| `constraint "inh_check_constraint" of relation "invalid_check` | 1 | inherit |
| `role "regress_priv_user2" does not exist` | 1 | privileges |
| `role "regress_priv_user3" does not exist` | 1 | privileges |
| `role "regress_priv_user5" does not exist` | 1 | privileges |
| `role "pg_read_all_data" does not exist` | 1 | privileges |
| `role "pg_write_all_data" does not exist` | 1 | privileges |
| `role "regress_schemauser_renamed" does not exist` | 1 | privileges |
| `role "regress_priv_group1" does not exist` | 1 | privileges |
| `role "pg_read_all_stats" does not exist` | 1 | privileges |
| `constraint "target_pkey" of relation "target" does not exist` | 1 | merge |
| `role "regress_tenant" does not exist` | 1 | create_role |

#### unexpected error 22023 (top 25)

| key | count | suites |
|---|---:|---|
| `invalid value for parameter "enable_bitmapscan": "f"` | 6 | multirangetypes, rangetypes |
| `unrecognized parameter "autovacuum_enabled"` | 5 | alter_table, groupingsets, privileges |
| `invalid value for parameter "enable_hashjoin": "f"` | 4 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_mergejoin": "f"` | 4 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_nestloop": "f"` | 4 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_seqscan": "f"` | 4 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_seqscan": "t"` | 3 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_indexscan": "f"` | 3 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_indexscan": "t"` | 3 | multirangetypes, rangetypes |
| `unrecognized parameter "vacuum_index_cleanup"` | 3 | vacuum |
| `invalid value for parameter "enable_nestloop": "t"` | 2 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_hashjoin": "t"` | 2 | multirangetypes, rangetypes |
| `invalid value for parameter "enable_mergejoin": "t"` | 2 | multirangetypes, rangetypes |
| `unit "isoyear" not supported for extract` | 2 | date |
| `unit "isodow" not supported for extract` | 2 | date |
| `invalid value for parameter "min_parallel_table_scan_size": ` | 2 | incremental_sort, partition_aggregate |
| `invalid value for parameter "min_parallel_index_scan_size": ` | 2 | incremental_sort, vacuum_parallel |
| `unrecognized parameter "fillfactor"` | 2 | alter_table, reloptions |
| `unit "julian" not supported for extract` | 1 | date |
| `precision 7 out of range for current_timestamp` | 1 | expressions |
| `START value (-32768) cannot be less than MINVALUE (1) or gre` | 1 | sequence |

#### unexpected error 42501 (top 25)

| key | count | suites |
|---|---:|---|
| `permission denied: must be owner of table "vacowned_part2"` | 12 | vacuum |
| `permission denied: must be superuser to manage roles` | 11 | privileges, rowsecurity |
| `permission denied for column "a" of table "z1"` | 10 | rowsecurity |
| `permission denied: must be owner of table "maintain_test"` | 6 | privileges |
| `permission denied: must be owner of table "vacowned"` | 3 | vacuum |
| `permission denied: must be owner of table "vacowned_parted"` | 3 | vacuum |
| `permission denied: must be owner of table "vacowned_part1"` | 3 | vacuum |
| `permission denied: must be owner of table "grantor_test" to ` | 2 | privileges |
| `permission denied: must be owner of table "grantor_test2" to` | 1 | privileges |
| `permission denied: must be owner of table "grantor_test3" to` | 1 | privileges |
| `permission denied: must be owner of table "t1"` | 1 | rowsecurity |

#### unexpected error 3B001 (top 25)

| key | count | suites |
|---|---:|---|
| `no such savepoint "settings"` | 30 | join_hash, select_parallel |
| `no such savepoint "q"` | 5 | rowsecurity |
| `no such savepoint "x"` | 3 | enum |
| `no such savepoint "a"` | 2 | plpgsql, prepared_xacts |
| `no such savepoint "s1"` | 2 | privileges |
| `no such savepoint "b"` | 2 | stats_rewrite |
| `no such savepoint "f"` | 1 | constraints |
| `no such savepoint "savept1"` | 1 | foreign_key |
| `no such savepoint "fp_cid"` | 1 | foreign_key |
| `no such savepoint "rescue_me"` | 1 | temp |
| `no such savepoint "save"` | 1 | sequence |
| `no such savepoint "foo"` | 1 | stats |
| `no such savepoint "sp2"` | 1 | stats |

#### catalog column (top 25)

| key | count | suites |
|---|---:|---|
| `rolreplication` | 8 | roleattributes |
| `reloptions` | 7 | reloptions |
| `relfilenode` | 6 | alter_table, cluster, tablespace, vacuum |
| `relpersistence` | 4 | create_table, identity |
| `relnamespace` | 3 | create_view, namespace |
| `relreplident` | 3 | replica_identity |
| `reltuples` | 2 | vacuum |
| `attgenerated` | 2 | generated_stored, generated_virtual |
| `attcompression` | 2 | create_table_like |
| `attcollation` | 1 | opr_sanity |
| `atttypid` | 1 | misc_sanity |
| `relpages` | 1 | stats_import |
| `attislocal` | 1 | create_table |
| `relhasindex` | 1 | vacuum |
| `relam` | 1 | create_am |
| `tableoid` | 1 | update |
| `attidentity` | 1 | identity |
| `attinhcount` | 1 | alter_table |
| `histogram_bounds` | 1 | polymorphism |
| `relhassubclass` | 1 | indexing |
| `atthasmissing` | 1 | fast_default |

#### other (top 25)

| key | count | suites |
|---|---:|---|
| `COPY error ['42P01']` | 20 | bit, copy, copy2, create_view, domain, enum +4 |
| `expected error, COPY succeeded` | 11 | copy, copy2, privileges, rowsecurity |
| `data load: COPY error ['42P01']` | 9 | create_index, create_view, jsonb, test_setup, tsearch |
| `unexpected extra result set (tag='SELECT 16' rows=16)` | 2 | partition_prune |
| `unexpected extra result set (tag='SELECT 1' rows=1)` | 1 | create_function_sql |
| `unexpected extra result set (tag='' rows=0 err=25P02)` | 1 | privileges |
| `COPY error ['22P04']` | 1 | rowsecurity |
| `unexpected extra result set (tag='SELECT 0' rows=0)` | 1 | misc_functions |
| `COPY error ['25P02']` | 1 | copy2 |
| `COPY error ['22P02']` | 1 | copy2 |

#### unexpected error 2BP01 (top 25)

| key | count | suites |
|---|---:|---|
| `cannot truncate a table referenced in a foreign key constrai` | 5 | truncate |
| `cannot drop column col1 of table idxpart because idxpart_col` | 3 | indexing |
| `cannot drop column d of table tt2 because v1 depends on it` | 1 | create_view |
| `cannot drop column c of table tt5 because vv1 depends on it` | 1 | create_view |
| `cannot drop column xx of table tt9 because vv5 depends on it` | 1 | create_view |
| `cannot drop column c3 of table tbl because tbl_idx depends o` | 1 | index_including |
| `cannot drop table inh_fk_1 because inh_fk_2_y_fkey depends o` | 1 | inherit |
| `cannot drop view rw_view1 because view rw_view2 depends on i` | 1 | updatable_views |
| `cannot drop table base_tbl because rw_view1 depends on it` | 1 | updatable_views |
| `cannot drop table uv_pt because uv_ptv depends on it` | 1 | updatable_views |
| `role "regress_sro_user" cannot be dropped because it owns ta` | 1 | privileges |
| `role "regress_rls_bob" cannot be dropped because it owns tab` | 1 | rowsecurity |
| `role "regress_rls_group2" cannot be dropped because it owns ` | 1 | rowsecurity |
| `cannot drop table pktable because fktable_ftest1_fkey depend` | 1 | foreign_key |
| `cannot drop table pktable2 because fktable2_d_fkey depends o` | 1 | foreign_key |
| `cannot drop table fk_notpartitioned_pk because fk_partitione` | 1 | foreign_key |
| `cannot drop table fk_partitioned_pk_6 because fk_partitioned` | 1 | foreign_key |
| `cannot drop table pt because  depends on it` | 1 | foreign_key |
| `cannot drop table truncprim because truncpart_a_fkey depends` | 1 | truncate |
| `cannot drop table trunc_a because ref_c_a_fkey depends on it` | 1 | truncate |
| `cannot drop column id of table atacc1 because atacc_oid1 dep` | 1 | alter_table |
| `cannot drop column value of table atacc1 because constraint ` | 1 | alter_table |
| `cannot drop table check_fk_presence_1 because check_fk_prese` | 1 | alter_table |
| `cannot drop column othercol of table old_system_table becaus` | 1 | alter_table |
| `cannot drop table attbl because atref_c1_fkey depends on it` | 1 | alter_table |

#### unexpected error 42804 (top 25)

| key | count | suites |
|---|---:|---|
| `column "text_field" is of type text but expression is of typ` | 5 | uuid |
| `column "d" is of type text but expression is of type timesta` | 2 | timestamp, timestamptz |
| `column "column1" is of type text but expression is of type r` | 2 | create_view |
| `column "tableoid" is of type regclass but expression is of t` | 2 | partition_prune, rowsecurity |
| `column "a" is of type record but expression is of type text` | 2 | partition_prune |
| `ORDER BY cannot compare pg_lsn with pg_lsn` | 1 | pg_lsn |
| `column "data" is of type text but expression is of type inte` | 1 | insert_conflict |
| `column "partkey" is of type timestamp without time zone but ` | 1 | create_table |
| `column "a" in child table "notnull_tbl1" must be marked NOT ` | 1 | constraints |
| `column "r" is of type regclass but expression is of type int` | 1 | inherit |
| `column "f1" in child table "inh_child2" must be marked NOT N` | 1 | inherit |
| `WITHIN GROUP types character varying and text cannot be matc` | 1 | aggregates |
| `WITHIN GROUP types text and integer cannot be matched` | 1 | aggregates |
| `WITHIN GROUP types text and name cannot be matched` | 1 | aggregates |
| `ORDER BY cannot compare integer[] with integer[]` | 1 | arrays |
| `ARRAY types Int and Text cannot be matched` | 1 | arrays |
| `column "b" is of type integer but expression is of type nume` | 1 | update |
| `child table is missing column "junk1"` | 1 | rowsecurity |
| `column "f_float4" is of type real but expression is of type ` | 1 | window |
| `column "name" is of type text but expression is of type inte` | 1 | plpgsql |
| `column "f1" is of type record[] but expression is of type te` | 1 | domain |
| `column "a" is of type integer but expression is of type nume` | 1 | indexing |

#### unexpected error 42710 (top 25)

| key | count | suites |
|---|---:|---|
| `constraint "" already exists` | 9 | alter_table, constraints, foreign_key, inherit, publication |
| `type "testdomain" already exists` | 3 | collate.icu.utf8, collate.linux.utf8, collate.windows.win1252 |
| `type "dcomptype" already exists` | 2 | domain |
| `constraint "ditto" already exists` | 1 | constraints |
| `role "regress_priv_user1" already exists` | 1 | privileges |
| `role "regress_grantor1" already exists` | 1 | privileges |
| `type "gtestdomain1" already exists` | 1 | generated_virtual |
| `type "gtestdomainnn" already exists` | 1 | generated_virtual |
| `constraint "c1" of domain "posint" already exists` | 1 | domain |
| `constraint "c2" of domain "posint" already exists` | 1 | domain |
| `type "testdomain1" already exists` | 1 | domain |
| `type "testtype1" already exists` | 1 | rowtypes |
| `type "testtype3" already exists` | 1 | rowtypes |
| `type "testtype5" already exists` | 1 | rowtypes |

#### unexpected error 23503 (top 25)

| key | count | suites |
|---|---:|---|
| `insert or update on table "fktable" violates foreign key con` | 6 | alter_table, foreign_key |
| `insert or update on table "pktable" violates foreign key con` | 4 | foreign_key |
| `insert or update on table "cc" violates foreign key constrai` | 2 | foreign_key |
| `insert or update on table "r2" violates foreign key constrai` | 1 | rowsecurity |
| `insert or update on table "tasks" violates foreign key const` | 1 | foreign_key |
| `insert or update on table "selfref" violates foreign key con` | 1 | foreign_key |
| `insert or update on table "parted_self_fk" violates foreign ` | 1 | foreign_key |
| `insert or update on table "dropfk" violates foreign key cons` | 1 | foreign_key |
| `delete on table "pt" violates foreign key constraint "" on t` | 1 | foreign_key |
| `insert or update on table "fkpart13_t3" violates foreign key` | 1 | foreign_key |
| `insert or update on table "fkpart13_t2_p1" violates foreign ` | 1 | foreign_key |
| `insert or update on table "fp_fk_cci" violates foreign key c` | 1 | foreign_key |
| `insert or update on table "fp_reentry_fk" violates foreign k` | 1 | foreign_key |
| `insert or update on table "ref_b" violates foreign key const` | 1 | truncate |
| `insert or update on table "ref_c" violates foreign key const` | 1 | truncate |

#### server crash/timeout (top 25)

| key | count | suites |
|---|---:|---|
| `CREATE TABLE` | 4 | alter_table, foreign_key, indexing |
| `CREATE TRIGGER` | 3 | copy2, rangefuncs, with |
| `ALTER TABLE` | 2 | foreign_key |
| `SELECT COUNT(*),AVG(T2.UNIQUE1)` | 2 | memoize |
| `SELECT COUNT(*),` | 2 | memoize |
| `SELECT 0\;` | 1 | copyselect |
| `TRIGGERS` | 1 | triggers |
| `SELECT INFINITE_RECURSE();` | 1 | infinite_recurse |
| `SELECT T1.TEN,` | 1 | subselect |
| `EXECUTE FOO(TRUE);` | 1 | join |
| `SELECT CTID` | 1 | join |
| `SELECT AVG((SELECT` | 1 | aggregates |
| `GRANT REFERENCES` | 1 | foreign_key |
| `REVOKE ALL` | 1 | foreign_key |
| `SELECT AVG(T1.A),` | 1 | partition_join |

#### unexpected error 42803 (top 25)

| key | count | suites |
|---|---:|---|
| `arguments to GROUPING must be grouping expressions of the as` | 5 | groupingsets |
| `column "b" must appear in the GROUP BY clause or be used in ` | 4 | groupingsets |
| `column "a" must appear in the GROUP BY clause or be used in ` | 3 | groupingsets, select_implicit |
| `aggregates are not allowed in WHERE clause` | 3 | aggregates, groupingsets |
| `SELECT DISTINCT ON expressions must match initial ORDER BY e` | 1 | select_distinct_on |
| `column "c" must appear in the GROUP BY clause or be used in ` | 1 | select_implicit |
| `aggregate functions are not allowed in FROM clause of their ` | 1 | aggregates |
| `FILTER specified, but any_value is not an aggregate function` | 1 | aggregates |
| `column "y" must appear in the GROUP BY clause or be used in ` | 1 | groupingsets |

#### unexpected error 22008 (top 25)

| key | count | suites |
|---|---:|---|
| `timestamp out of range` | 2 | timestamptz |
| `input "2011-12-18  23:38:15" does not match format "YYYY-MM-` | 2 | horology |
| `date/time field value out of range: hour 24` | 1 | timestamp |
| `input "2011$03!18 23_38_15" does not match format "YYYY-MM-D` | 1 | horology |
| `input "1985 \\ 12" does not match format "YYYY \\\\ DD"` | 1 | horology |
| `input "20000-1116" does not match format "YYYY-MMDD"` | 1 | horology |
| `input "2005 03 02" does not match format "YYYYMMDD"` | 1 | horology |
| `input " 2005 03 02" does not match format "YYYYMMDD"` | 1 | horology |
| `input "-44-02-01" does not match format "YYYY-MM-DD"` | 1 | horology |
| `input "-44-02-01 11:12:13" does not match format "YYYY-MM-DD` | 1 | horology |
| `input "2011-12-18 23:38:15" does not match format "YYYY-MM-D` | 1 | horology |
| `input "2011-12-18   23:38:15" does not match format "YYYY-MM` | 1 | horology |
| `input "2011 12  18" does not match format "YYYY MM DD"` | 1 | horology |
| `input "2011 12  18" does not match format "YYYY MM   DD"` | 1 | horology |
| `input "2011 12 18" does not match format "YYYY  MM DD"` | 1 | horology |
| `input "2011   12 18" does not match format "YYYY  MM DD"` | 1 | horology |
| `date/time field value out of range: 86 in input "2015-02-11 ` | 1 | horology |

#### unexpected error 26000 (top 25)

| key | count | suites |
|---|---:|---|
| `prepared statement "get_nnconstraint_info" does not exist` | 14 | constraints |
| `prepared statement "foo" does not exist` | 1 | join |
| `prepared statement "foom" does not exist` | 1 | merge |
| `prepared statement "foom2" does not exist` | 1 | merge |
| `prepared statement "tenk1_count" does not exist` | 1 | select_parallel |
| `prepared statement "pp" does not exist` | 1 | xml |

#### unexpected error 23505 (top 25)

| key | count | suites |
|---|---:|---|
| `duplicate key value violates unique constraint "hats_pkey"` | 8 | rules |
| `duplicate key value violates unique constraint "pp_pkey"` | 3 | foreign_key |
| `duplicate key value violates unique constraint "pktable_pkey` | 2 | foreign_key |
| `duplicate key value violates unique constraint "atest5_four_` | 1 | privileges |
| `duplicate key value violates unique constraint "dcomptable_d` | 1 | domain |
| `duplicate key value violates unique constraint "trunc_c_pkey` | 1 | truncate |
| `duplicate key value violates unique constraint "truncate_a_p` | 1 | truncate |
| `duplicate key value violates unique constraint "onek_unique1` | 1 | alter_table |

#### unexpected error 42821 (top 25)

| key | count | suites |
|---|---:|---|
| `op ANY/ALL (array) requires array on right side, not text` | 16 | arrays, create_index |

#### unexpected error 42846 (top 25)

| key | count | suites |
|---|---:|---|
| `cannot cast to composite type here` | 3 | rowtypes |
| `cannot cast type Numeric(None) to rngfunc_type` | 2 | rangefuncs |
| `cannot cast type uuid to bytea` | 1 | uuid |
| `cannot cast type bytea to uuid` | 1 | uuid |
| `cannot cast type record to casttesttype` | 1 | create_cast |
| `cannot cast type integer[] to integer` | 1 | domain |
| `cannot cast type Int to rngfunc_type` | 1 | rangefuncs |
| `cannot cast type Array(Record) to price_input` | 1 | rowtypes |

#### unexpected error 42723 (top 25)

| key | count | suites |
|---|---:|---|
| `function "mylt" already exists with same argument types` | 2 | collate.linux.utf8, collate.windows.win1252 |
| `function "mylt_noninline" already exists with same argument ` | 2 | collate.linux.utf8, collate.windows.win1252 |
| `function "mylt_plpgsql" already exists with same argument ty` | 2 | collate.linux.utf8, collate.windows.win1252 |
| `function "alt_func1" already exists with same argument types` | 1 | alter_generic |
| `function "alt_func2" already exists with same argument types` | 1 | alter_generic |
| `function "gtest_trigger_func3" already exists with same argu` | 1 | generated_virtual |
| `function "gtest_trigger_func4" already exists with same argu` | 1 | generated_virtual |
| `function "stable_one" already exists with same argument type` | 1 | partition_prune |

#### unexpected error 42701 (top 25)

| key | count | suites |
|---|---:|---|
| `column "c1" specified more than once` | 1 | create_index |
| `column "c" of relation "tt3" already exists` | 1 | create_view |
| `column "value" of relation "atacc1" already exists` | 1 | alter_table |
| `column "f2" of relation "recur1" already exists` | 1 | alter_table |
| `column "suffix" of relation "fullname" already exists` | 1 | rowtypes |
| `column "a" specified more than once` | 1 | indexing |

#### unexpected error 42830 (top 25)

| key | count | suites |
|---|---:|---|
| `there is no unique constraint matching given keys for refere` | 4 | foreign_key |
| `there is no primary key for referenced table "concur_reindex` | 1 | create_index |
| `there is no primary key for referenced table "inhz"` | 1 | create_table_like |

#### unexpected error 42809 (top 25)

| key | count | suites |
|---|---:|---|
| `WITHIN GROUP is required for ordered-set aggregate percent_r` | 2 | window |
| `WITHIN GROUP is required for ordered-set aggregate cume_dist` | 2 | window |
| `WITHIN GROUP specified, but test_rank is not an aggregate fu` | 1 | aggregates |
| `WITHIN GROUP specified, but test_percentile_disc is not an a` | 1 | aggregates |

#### unexpected error 42P13 (top 25)

| key | count | suites |
|---|---:|---|
| `return type mismatch in function declared to return hobbies_` | 2 | misc |
| `return type mismatch in function declared to return rngfunc_` | 2 | rangefuncs |
| `return type mismatch in function declared to return int` | 1 | plpgsql |
| `return type mismatch in function declared to return date` | 1 | plpgsql |

#### unexpected error 23502 (top 25)

| key | count | suites |
|---|---:|---|
| `null value in column "a" of relation "notnull_tbl1" violates` | 1 | constraints |
| `null value in column "id" of relation "itest14" violates not` | 1 | identity |
| `null value in column "test" of relation "atacc1" violates no` | 1 | alter_table |
| `null value in column "a" of relation "atacc1" violates not-n` | 1 | alter_table |

#### unexpected error 42P10 (top 25)

| key | count | suites |
|---|---:|---|
| `there is no unique or exclusion constraint matching the ON C` | 2 | insert_conflict |

#### unexpected error 42P16 (top 25)

| key | count | suites |
|---|---:|---|
| `relation "notnull_tbl1" would be inherited from more than on` | 1 | constraints |
| `relation "atacc2" would be inherited from more than once` | 1 | alter_table |

#### unexpected error 42712 (top 25)

| key | count | suites |
|---|---:|---|
| `table name "t2" specified more than once` | 1 | rowsecurity |
| `table name "t1" specified more than once` | 1 | rowsecurity |

#### unexpected error 25001 (top 25)

| key | count | suites |
|---|---:|---|
| `SET LOCAL can only be used within a transaction block` | 2 | guc |

#### unexpected error 42883 (top 25)

| key | count | suites |
|---|---:|---|
| `OVER specified, but jsonb_object_agg_unique is not a window ` | 1 | jsonb |
| `OVER specified, but jsonb_object_agg_unique_strict is not a ` | 1 | jsonb |

#### unexpected error 25006 (top 25)

| key | count | suites |
|---|---:|---|
| `cannot execute nextval() in a read-only transaction` | 1 | sequence |
| `cannot execute setval() in a read-only transaction` | 1 | sequence |

#### unexpected error 54001 (top 25)

| key | count | suites |
|---|---:|---|
| `recursive CTE "t" exceeded the 10000-iteration limit (cyclic` | 1 | with |
| `recursive CTE "q" exceeded the 10000-iteration limit (cyclic` | 1 | with |

#### unexpected error 42P19 (top 25)

| key | count | suites |
|---|---:|---|
| `recursive query "t" must not contain data-modifying statemen` | 1 | with |
| `recursive query "t2" must not contain data-modifying stateme` | 1 | with |

#### unexpected error 21000 (top 25)

| key | count | suites |
|---|---:|---|
| `more than one row returned by a subquery used as an expressi` | 2 | with |

#### unexpected error 2202E (top 25)

| key | count | suites |
|---|---:|---|
| `cannot concatenate incompatible arrays` | 1 | arrays |

#### unexpected error 22004 (top 25)

| key | count | suites |
|---|---:|---|
| `ntile argument must not be null` | 1 | window |

#### unexpected error 42P08 (top 25)

| key | count | suites |
|---|---:|---|
| `cannot determine type of empty array` | 1 | rangefuncs |

#### unexpected error 40001 (top 25)

| key | count | suites |
|---|---:|---|
| `could not serialize access due to concurrent update: concurr` | 1 | stats |

#### unexpected error 22003 (top 25)

| key | count | suites |
|---|---:|---|
| `value overflows integer` | 1 | compression |
