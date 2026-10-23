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

Steady state costs one point get per statement. Step 3 needs a second get only when the
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
