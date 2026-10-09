# WAL fixture: DROP COLUMN before an indexed column (written by v1.79)

Produced by the v1.79 server (main at 96e426a) from:

```sql
CREATE TABLE t (b char, a int UNIQUE);
INSERT INTO t VALUES ('x', 3);
ALTER TABLE t DROP b;
INSERT INTO t VALUES (4);
```

v1.79 and earlier logged the DROP COLUMN as `AlterTable` → `InsertRows` (rewritten,
one-column rows) → `DropIndex` → `CreateIndex` (shifted positions). Replaying
`InsertRows` while the stale index (`t_a_key` on position 1) was still live
panicked in `Database::index_insert_row`, and the data directory never opened
again. Newer servers log the DropIndex first, but must still open logs like
this one. Used by `wal::tests::v180_replays_v179_drop_column_wal`.
