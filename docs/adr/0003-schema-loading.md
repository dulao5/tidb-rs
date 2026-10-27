# ADR 0003: Schema loading for the read-only node

- Status: proposed (2026-10-23)
- Context: ADR 0001, "Correctness requirements for the read-only node". Code: `crates/meta`
  (`MetaReader`, `InfoSchema`, `SchemaCache`), `crates/kv`.

## Decision

The read-only node does not join TiDB's schema-sync protocol. Instead, every statement uses
the schema that is visible at its own `start_ts`:

1. Get `start_ts` from PD TSO.
2. At `start_ts`, read the schema version key `m SchemaVersionKey` (one point get).
3. Compute the *effective* version as Go's `Mutator.GetSchemaVersionWithNonEmptyDiff`
   does: if the diff of that version (`m Diff:{v}`) does not exist yet, use `v - 1`.
4. If an `InfoSchema` of the effective version is cached, use it. Otherwise read all
   databases (`m DBs` hash) and their tables (`m DB:{id}` hashes) at `start_ts`, build an
   `InfoSchema`, and cache it under the effective version (8 versions are kept).
5. Plan the statement with that `InfoSchema` and read data at the same `start_ts`.

Steady state costs one point get per statement (run concurrently with the data read; see
"Optimistic schema check"). Step 3 needs a second get only when the
cache cannot already prove that the diff of the version exists at `start_ts`. A cache entry
records the earliest timestamp at which its diff was seen. Any later `start_ts` with the same
version key must also see that diff. Reloads are full reloads; schema diffs are not applied
incrementally.

## Why this is consistent

- **Meta and data come from one snapshot.** Schema and rows are read at the same MVCC
  timestamp, so the statement sees one committed state of the cluster. A DDL that commits
  after `start_ts` is invisible to both. A DDL that commits before `start_ts` is visible to
  both.
- **Why the effective version.** The DDL owner allocates the next version in its own small
  transaction (`schemaVersionManager`). It writes the meta changes together with that
  version's `SchemaDiff` in the job's transaction afterwards. So at some `start_ts` the version
  key can be `v` while the meta still shows version `v - 1`. All meta changes of one version
  commit atomically with its diff. The effective version therefore identifies the meta
  content exactly, which makes it a safe cache key.
- **Data under an intermediate DDL state is readable.** Online DDL (F1-style) moves an
  element through states such as delete-only, write-only, reorg and public. Data committed
  under state `s` is valid under the adjacent states `s - 1` and `s + 1`. Writers can commit
  with schema `v - 1` after `v` is published, but only within that two-version window. A
  reader holding version `v` at `start_ts` therefore only meets data that `v` can interpret.
  Columns not yet public are not read. Rows written before an `ADD COLUMN` are filled with the
  column's *origin default*, which is pushed down as `ColumnInfo.default_val`, as Go does.
- **Concurrent statements with older timestamps** can still need an older version. They do
  not evict or overwrite newer cache entries, because entries are keyed by version and
  several are kept.

## Optimistic schema check (2026-10-27)

Steps 2-5 above are serial: the version get must finish before planning and the data read
can start. Sessions now overlap them (`SchemaCache::candidate` / `check_at`,
`tidb_session::Session::optimistic_read`):

1. Plan the statement with the newest cached `InfoSchema` (the *candidate*, version `c`).
2. With the statement's `start_ts`, issue the version get (step 2) and the data read
   **concurrently**, both at `start_ts`.
3. Accept the read's result (rows or error) only if the version key at `start_ts` is
   `raw == c` and the cache entry of `c` records `diff_seen_at <= start_ts`. This is exactly
   the condition under which step 4 returns that cache entry after one get. Otherwise discard
   the result and continue with step 3 using the `raw` just read (`info_schema_with_raw`),
   then plan and read again at the same `start_ts`. If planning with the candidate fails, take
   the full path too: the error may come from a stale schema.

Why the guarantee is unchanged: an accepted statement used the schema that the serial path
would have chosen for the same `start_ts` (version `c` identifies the meta content exactly;
see "Why the effective version"), the same plan, and a read at the same `start_ts`. Its
result is therefore identical. A result computed with any other schema is never returned,
not even as an error. The only cost of a wrong guess is a second read after a DDL; the
statement then behaves exactly as before. Statements whose `start_ts` is older than the
newest cached version (a concurrent statement already loaded a newer one) are rejected and
then served from the cache by the full path.

**Shared version reads.** Concurrent statements share the version-key read. The check for a
statement at `start_ts` may use a read at any `read_ts >= start_ts`. It uses the newest
completed read if that one's `read_ts >= start_ts`, otherwise joins the in-flight read if its
`read_ts` is large enough, otherwise starts a read at the newest `start_ts` any statement has
passed to the check (a TSO that PD has issued). The acceptance rule is the same: `raw == c`
and `diff_seen_at <= start_ts`. It stays exact because the version key only ever increases
in commit order: every DDL increments it inside a transaction, and `FLASHBACK CLUSTER`
rewrites only the `m DBs` / `m DB:{id}` keys and then increments the version too. The key is
`c` at `diff_seen_at`, because that is how `diff_seen_at` is recorded, and `c` at
`read_ts`. So it is `c` at every timestamp in between, `start_ts` included, and the diff of
`c` is visible there. When the read was not at exactly `start_ts`, a rejected statement
re-reads the version key at its own `start_ts` on the full path. At 16 sysbench threads this
cuts the version reads to about 0.4 per statement.

The TSO request is also sent as soon as the statement is known to read a table (after parsing
for COM_QUERY, before binding parameters for COM_STMT_EXECUTE), and awaited only after
planning. Statements without a table, `SET` and `USE` do not request one early.

## What this does not cover

- **No schema-version sync or MDL.** The node does not register in etcd
  (`/tidb/ddl/all_schema_versions`) and does not take metadata locks. The DDL owner therefore
  does not wait for it. This is safe for snapshot reads, because the reads never use a schema
  newer or older than their snapshot. It would **not** be safe for writes. Go TiDB needs sync
  or MDL so that its *writes* never lag two versions behind. Writes must not be added on top
  of this design.
- **No schema lease.** Go TiDB validates a cached schema against a lease and fails a
  statement when it cannot reload within the lease. We read the version at every statement,
  so freshness depends only on `start_ts`, not on a background loop.
- **GC.** If `start_ts` is older than the GC safe point, meta reads fail like any other
  stale read. That cannot happen with a fresh TSO.
- **Cost of full reloads.** Every new version reloads every table's JSON. That is fine for
  sysbench-sized catalogs, but large catalogs need `SchemaDiff`-based incremental loading, as
  in Go's `infoschema.Builder`.
- **Visibility rules.** Only `Public` databases, tables and columns are exposed. Tables in
  intermediate states are treated as not existing, which matches TiDB's reads. Partitioned
  tables, views, sequences, temporary tables and common-handle tables are loaded but rejected
  by `TableInfo::check_int_handle_readable`.
- **Placement, resource groups, policies, the `mysql.tidb` settings** (for example
  `new_collation_enabled`) and global variables are not read here. `DagContext` currently
  assumes new collations are enabled.
- **Keyspaces (API v2 / next-gen).** Meta keys are read without a keyspace prefix (classic
  TiDB).
